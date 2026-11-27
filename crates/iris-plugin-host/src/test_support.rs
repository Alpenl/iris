//! Minimal white-box test harnesses gated behind the non-default `test-support`
//! feature for integration binaries that require process-level isolation
//! (`#[global_allocator]` in `plugin_v2_read_memory`) or freshly built candidate
//! official `.irisplugin` archives (`plugin_v2_official`).
use crate::{
    actor::{Actor, Executor, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    hub::{Config, Hub},
    installation::{Approval, Installer},
    instance::Blob,
    ledger::{Ledger, Resource},
    package::{Package, Provenance},
    pool::IoPool,
    product::PluginHost,
    services::{Resources, Setup},
};
use iris_api::v2::{ErrorCode, Generation, Role, task::Task};
use iris_base::rt::CancelToken;
use iris_core::vault::CredentialVault;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

struct NoIo;

impl Executor for NoIo {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        _: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }

    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }

    fn stop(&self) {}
}

/// Isolated `Role::Stream` actor probe for `plugin_v2_read_memory`'s allocation test.
pub struct StreamReadMemoryProbe {
    actor: Actor,
    ledger: Ledger,
}

impl StreamReadMemoryProbe {
    pub fn spawn(wasm_bytes: &[u8]) -> Result<Self, ErrorCode> {
        let engine = RuntimeEngine::new()?;
        let module = engine.compile(wasm_bytes, &CancelToken::new())?;
        let ledger = Ledger::default();
        let actor = Actor::spawn(Spawn {
            engine,
            module,
            role: Role::Stream,
            owner: "read-memory-regression".into(),
            grants: BTreeSet::new(),
            ledger: ledger.clone(),
            io: IoPool::new()?,
            executor: Arc::new(NoIo),
            generation: Generation::default(),
            wake: Arc::new(|| {}),
        })?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while actor.status() != Status::Ready {
            if Instant::now() >= deadline {
                return Err(ErrorCode::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        actor.send(br#"{"event":"init","role":"stream"}"#.to_vec())?;
        while !actor.initialized() {
            if Instant::now() >= deadline {
                return Err(ErrorCode::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(Self { actor, ledger })
    }

    pub fn read_single_byte(&self, offset: u64, timeout: Duration) -> Result<Vec<u8>, ErrorCode> {
        let ticket = self.actor.read("resource".into(), offset, 1, None)?;
        let result = ticket.receive_cancellable(&CancelToken::new(), timeout)?;
        Ok(result.blob.bytes.clone())
    }

    pub fn host_bytes_used(&self) -> u64 {
        self.ledger.used(None, Resource::HostBytes)
    }

    pub fn stream_reads_used(&self) -> u64 {
        self.ledger.used(None, Resource::StreamRead)
    }

    pub fn stop(&self) {
        self.actor.stop();
    }

    pub fn is_quiescent(&self) -> bool {
        self.actor.is_quiescent()
    }
}

/// Opens an isolated [`PluginHost`] pre-populated with a freshly built candidate
/// official (`iris.*`) `.irisplugin` archive pinned by its computed content digest.
pub fn open_host_with_bundled_package(
    data_dir: &Path,
    plugin_id: &str,
    package_bytes: &[u8],
) -> Result<PluginHost, ErrorCode> {
    let resources = Resources::new()?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(package_bytes))
        .map_err(|_| ErrorCode::InvalidArgument)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    for name in ["plugin.json", "plugin.wasm"] {
        let mut content = Vec::new();
        archive
            .by_name(name)
            .map_err(|_| ErrorCode::InvalidArgument)?
            .read_to_end(&mut content)
            .map_err(|_| ErrorCode::IoError)?;
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(&(content.len() as u64).to_le_bytes());
        hash.update(&content);
    }
    let digest: [u8; 32] = hash
        .finish()
        .as_ref()
        .try_into()
        .map_err(|_| ErrorCode::Internal)?;
    let index = BTreeMap::from([(plugin_id.to_owned(), digest)]);
    let package = Package::from_zip(package_bytes, Provenance::Bundled(&index))?;
    let mut installer =
        Installer::with_bundled(data_dir.join("packages"), resources.ledger.clone(), index);
    installer.install_bundled(
        package_bytes,
        Approval {
            digest,
            grants: package.manifest.permissions.into_iter().collect(),
            network: package.manifest.network,
            previous: None,
        },
    )?;
    let hub = Hub::new(Config {
        bootstrap: None,
        installer,
        disabled: BTreeSet::new(),
        runtime: Setup {
            resources: resources.clone(),
            state_directory: data_dir.join("state"),
            temporary_directory: data_dir.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        },
    })?;
    Ok(PluginHost::from_parts(resources, hub))
}
