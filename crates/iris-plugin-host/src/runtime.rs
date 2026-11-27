//! Installation lifecycle shared by all host windows. Loading and window setup
//! run on the host coordinator worker; send/poll/stop never wait on I/O or join.
mod access;
mod control;
pub use access::{BackgroundResult, WindowAccess};
mod events;
mod playback;
mod window;
use super::{
    actor::{Actor, Spawn},
    engine::{CompiledModule, RuntimeEngine},
    files::FileExecutor,
    installation::{Installed, Installer},
    instance::next_id,
    json_error,
    ledger::{Ledger, Reservation, Resource},
    network::{NetworkGrant, consent::OriginStore},
    pool::IoPool,
    services::ControlServices,
    storage::Storage,
};
use iris_api::v2::{
    ErrorCode, Generation, HostEvent, Id, Manifest, Role, source::SourceDescriptor,
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::CancelToken;
use iris_core::vault::CredentialVault;
pub use playback::PlaybackPermit;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
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
}
#[derive(Clone)]
pub struct Setup {
    pub resources: Resources,
    pub state_directory: PathBuf,
    pub temporary_directory: PathBuf,
    pub vault: CredentialVault,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}
struct WindowState {
    generation: Generation,
    files: FileExecutor,
    media: HttpCancel,
    attachments: Arc<super::enhancement::AttachmentRegistry>,
}
struct LogRateLimiter {
    window_start: std::time::Instant,
    count: u32,
}
impl LogRateLimiter {
    const MAX_PER_SEC: u32 = 20;
    fn new() -> Self {
        Self {
            window_start: std::time::Instant::now(),
            count: 0,
        }
    }
    fn allow(&mut self) -> bool {
        let now = std::time::Instant::now();
        if now.duration_since(self.window_start) >= std::time::Duration::from_secs(1) {
            self.window_start = now;
            self.count = 0;
        }
        if self.count >= Self::MAX_PER_SEC {
            return false;
        }
        self.count += 1;
        true
    }
}
pub struct Control {
    control_id: Id,
    installed: Installed,
    manifest: Arc<Manifest>,
    module: Option<Arc<CompiledModule>>,
    setup: Setup,
    network: NetworkGrant,
    origins: OriginStore,
    services: Arc<ControlServices>,
    actor: Option<Actor>,
    window: Option<(Id, WindowState)>,
    sources: Vec<SourceDescriptor>,
    log_limiter: LogRateLimiter,
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
        cancel: &CancelToken,
    ) -> Result<Self, ErrorCode> {
        let package = installer.load(&installed)?;
        if cancel.is_cancelled() {
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
                iris_api::v2::Capability::NetHttp
                    | iris_api::v2::Capability::NetWs
                    | iris_api::v2::Capability::NetTcp
            )
        }) {
            origins.restore(&network, cancel)?;
        }
        let storage = Arc::new(Storage::new(
            setup.state_directory.clone(),
            installed.installation_id.clone(),
            setup.vault.clone(),
            setup.resources.ledger.clone(),
        )?);
        let services = Arc::new(ControlServices::new(
            installed.installation_id.clone(),
            setup.resources.ledger.clone(),
            network.clone(),
            storage,
            installed.grants.clone(),
            setup.temporary_directory.join(&installed.installation_id),
        )?);
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
            actor.send(
                iris_core::bounded_json::encode(&event, iris_api::v2::MESSAGE_LIMIT)
                    .map_err(json_error)?,
            )?;
        }
        if cancel.is_cancelled() {
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
            services,
            actor,
            window: None,
            sources,
            log_limiter: LogRateLimiter::new(),
            stopped: false,
            quarantined: None,
            _lease: lease,
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
