//! Installation lifecycle shared by all host windows. Loading and window setup
//! run on the host coordinator worker; send/poll/stop never wait on I/O or join.
mod access;
mod control;
pub use access::{BackgroundResult, WindowAccess};
mod events;
mod playback;
mod window;
use super::{
    actor::{Actor, Executor, Spawn, Status, Update},
    auth::{AuthExecutor, BrowserOpen},
    engine::{CompiledModule, RuntimeEngine},
    installation::{Installed, Installer},
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
    network::{NetworkExecutor, NetworkGrant, consent::OriginStore},
    pool::IoPool,
    services::{HostServices, WindowServices},
    storage::{Storage, StorageExecutor},
    tools::{
        ToolExecutor, ToolSetup,
        binding::{ToolKind, TrustedModel, TrustedTool},
    },
};
use crate::{vault::CredentialVault, vfs::HttpCancel};
use iris_api::v2::{
    ErrorCode, Generation, HostCommand, HostEvent, Id, Manifest, Role,
    source::{PlaybackSnapshot, SourceDescriptor},
};
pub use playback::PlaybackPermit;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
pub use window::{PreparedWindow, WindowPreparation};

#[derive(Clone)]
pub struct Resources {
    pub engine: RuntimeEngine,
    pub io: IoPool,
    pub ledger: Ledger,
}
impl Resources {
    pub fn new() -> Result<Self, ErrorCode> {
        Ok(Self {
            engine: RuntimeEngine::new()?,
            io: IoPool::new()?,
            ledger: Ledger::default(),
        })
    }
    /// The product must share these budgets/cache/pools across every window.
    pub fn global() -> Result<Self, ErrorCode> {
        static RESOURCES: OnceLock<Result<Resources, ErrorCode>> = OnceLock::new();
        RESOURCES.get_or_init(Self::new).clone()
    }
}
#[derive(Clone)]
pub struct Setup {
    pub resources: Resources,
    pub state_directory: PathBuf,
    pub temporary_directory: PathBuf,
    pub vault: CredentialVault,
    pub tools: BTreeMap<ToolKind, Arc<TrustedTool>>,
    pub models: BTreeMap<Id, Arc<TrustedModel>>,
    pub tool_catalog: super::tools::catalog::ToolCatalog,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}
struct WindowState {
    generation: Generation,
    services: HostServices,
    media: HttpCancel,
    attachments: Arc<super::enhancement::AttachmentRegistry>,
}
pub struct Control {
    control_id: Id,
    installed: Installed,
    manifest: Arc<Manifest>,
    module: Option<Arc<CompiledModule>>,
    setup: Setup,
    network: NetworkGrant,
    origins: OriginStore,
    storage: Arc<Storage>,
    services: Arc<WindowServices>,
    actor: Option<Actor>,
    windows: BTreeMap<Id, WindowState>,
    sources: Vec<SourceDescriptor>,
    stopped: bool,
    quarantined: Option<ErrorCode>,
    _lease: Reservation,
}
impl Control {
    /// Reads the current approved record again, verifies its exact package, then
    /// compiles and starts one control actor. Call only on a background worker.
    pub fn load(
        installer: &Installer,
        installed: Installed,
        setup: Setup,
        cancel: &AtomicBool,
    ) -> Result<Self, ErrorCode> {
        let package = installer.load(&installed)?;
        if cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        let module = package
            .wasm()
            .map(|bytes| setup.resources.engine.compile(bytes, cancel))
            .transpose()?;
        let manifest = Arc::new(package.manifest.clone());
        let lease = setup.resources.ledger.reserve(
            &installed.installation_id,
            &[(Resource::HostBytes, 2 << 20)],
        )?;
        let network = NetworkGrant::new(
            installed.installation_id.clone(),
            installed.grants.clone(),
            installed.network.clone(),
            &setup.resources.ledger,
        )?;
        let origins = OriginStore::new(
            setup.state_directory.clone(),
            installed.installation_id.clone(),
            setup.resources.ledger.clone(),
        )?;
        if installed.grants.iter().any(|c| {
            matches!(
                c,
                iris_api::v2::Capability::NetHttp | iris_api::v2::Capability::NetWs
            )
        }) {
            origins.restore(&network, &HttpCancel::default())?;
        }
        let storage = Arc::new(Storage::new(
            setup.state_directory.clone(),
            installed.installation_id.clone(),
            setup.vault.clone(),
            setup.resources.ledger.clone(),
        )?);
        let global = Self::make_services(
            &setup,
            &installed,
            &network,
            &storage,
            None,
            Arc::new(|_, _, _| Err(ErrorCode::Denied)),
        )?;
        let services = Arc::new(WindowServices::new(
            installed.installation_id.clone(),
            setup.resources.ledger.clone(),
            Arc::new(global),
        ));
        let actor = module
            .as_ref()
            .map(|module| {
                Actor::spawn(Spawn {
                    engine: setup.resources.engine.clone(),
                    module: module.clone(),
                    role: Role::Control,
                    owner: installed.installation_id.clone(),
                    grants: installed.grants.clone(),
                    ledger: setup.resources.ledger.clone(),
                    io: setup.resources.io.clone(),
                    executor: services.clone(),
                    generation: Generation::default(),
                    wake: setup.wake.clone(),
                })
            })
            .transpose()?;
        if let Some(actor) = &actor {
            let event = HostEvent::<serde_json::Value>::Init {
                protocol: 2,
                abi: 2,
                host_version: env!("CARGO_PKG_VERSION").into(),
                role: Role::Control,
                generation: Generation::default(),
                capabilities: installed.grants.clone(),
                features: BTreeSet::from(["window_scope".into(), "async_storage".into()]),
                snapshot: None,
            };
            actor.send(iris_plugin_sdk::json::encode(
                &event,
                iris_api::v2::MESSAGE_LIMIT,
            )?)?;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        let sources = if installed
            .grants
            .contains(&iris_api::v2::Capability::MediaSource)
        {
            manifest.sources.clone()
        } else {
            Vec::new()
        };
        Ok(Self {
            control_id: Id(next_id()?),
            installed,
            manifest,
            module,
            setup,
            network,
            origins,
            storage,
            services,
            actor,
            windows: BTreeMap::new(),
            sources,
            stopped: false,
            quarantined: None,
            _lease: lease,
        })
    }
    fn make_services(
        setup: &Setup,
        installed: &Installed,
        network: &NetworkGrant,
        storage: &Arc<Storage>,
        window: Option<Id>,
        browser: BrowserOpen,
    ) -> Result<HostServices, ErrorCode> {
        Ok(HostServices {
            network: NetworkExecutor::new(
                network.clone(),
                setup.resources.ledger.clone(),
                BTreeSet::new(),
            )?,
            auth: AuthExecutor::new(network.clone(), setup.resources.ledger.clone(), browser),
            files: ToolExecutor::new(ToolSetup {
                network: network.clone(),
                ledger: setup.resources.ledger.clone(),
                capabilities: installed.grants.clone(),
                temporary_directory: setup
                    .temporary_directory
                    .join(&installed.installation_id)
                    .join(window.map_or_else(|| "global".into(), |id| id.0.to_string())),
                tools: setup.tools.clone(),
                models: setup.models.clone(),
                catalog: setup.tool_catalog.clone(),
            })?,
            storage: StorageExecutor::new(
                storage.clone(),
                network.clone(),
                installed.grants.clone(),
            )?,
        })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn shared_manifest(&self) -> Arc<Manifest> {
        self.manifest.clone()
    }
    pub fn installed(&self) -> &Installed {
        &self.installed
    }
    pub fn sources(&self) -> &[SourceDescriptor] {
        &self.sources
    }
    pub fn network(&self) -> &NetworkGrant {
        &self.network
    }
    pub fn origins(&self) -> &OriginStore {
        &self.origins
    }
    pub fn window_files(&self, window: Id) -> Result<&ToolExecutor, ErrorCode> {
        self.live()?;
        self.windows
            .get(&window)
            .map(|w| &w.services.files)
            .ok_or(ErrorCode::StaleHandle)
    }
    pub fn new_window_id() -> Result<Id, ErrorCode> {
        next_id().map(Id)
    }
    fn live(&self) -> Result<(), ErrorCode> {
        if self.stopped {
            Err(self.quarantined.unwrap_or(ErrorCode::Cancelled))
        } else {
            Ok(())
        }
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
