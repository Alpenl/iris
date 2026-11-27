//! Explicit application-owned [`PluginHost`] context. Package discovery and
//! Wasm compilation run on background coordinator workers; an unavailable
//! plugin runtime never prevents native playback.
use super::{
    hub::{Client, Config, Hub},
    installation::Installer,
    ledger::Resource,
    runtime::{Resources, Setup},
    selection::HostAnnotationJob,
    ui::WindowSession,
};
use futures::channel::mpsc::UnboundedReceiver;
use iris_api::v2::{ErrorCode, Generation};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// Explicit host runtime context owned by the application or an isolated test.
/// Dropping or calling [`PluginHost::shutdown`] tears down the coordinator and
/// drains temporary artifact cleanup without process-wide mutable statics.
pub struct PluginHost {
    resources: Result<Resources, ErrorCode>,
    hub: Mutex<Option<Hub>>,
}

impl PluginHost {
    /// Initializes a host context backed by the application data directory and OS vault.
    pub fn open_default(disabled: &[String]) -> Self {
        let disabled_set: BTreeSet<String> = disabled.iter().cloned().collect();
        match Resources::new() {
            Ok(resources) => {
                let hub = make_hub(&resources, disabled_set).ok();
                Self {
                    resources: Ok(resources),
                    hub: Mutex::new(hub),
                }
            }
            Err(error) => Self {
                resources: Err(error),
                hub: Mutex::new(None),
            },
        }
    }

    /// Initializes an isolated host context rooted at `data_dir` and `vault`
    /// (`include_bundled` controls whether embedded official plugins are unpacked).
    pub fn open_in(
        data_dir: &std::path::Path,
        vault: iris_core::vault::CredentialVault,
        disabled: &[String],
        include_bundled: bool,
    ) -> Result<Self, ErrorCode> {
        let resources = Resources::new()?;
        let disabled_set: BTreeSet<String> = disabled.iter().cloned().collect();
        let hub = make_hub_in(
            &resources,
            data_dir.to_path_buf(),
            data_dir.join("plugins-v2"),
            vault,
            disabled_set,
            include_bundled,
        )?;
        Ok(Self {
            resources: Ok(resources),
            hub: Mutex::new(Some(hub)),
        })
    }

    /// Wraps caller-provided `Resources` and `Hub` for deterministic isolated tests.
    #[cfg(feature = "test-support")]
    pub(crate) fn from_parts(resources: Resources, hub: Hub) -> Self {
        Self {
            resources: Ok(resources),
            hub: Mutex::new(Some(hub)),
        }
    }

    pub(crate) fn resources(&self) -> Result<Resources, ErrorCode> {
        self.resources.clone()
    }

    /// Returns currently reserved `HostBytes` across this host instance.
    pub fn host_bytes_used(&self) -> u64 {
        self.resources
            .as_ref()
            .map_or(0, |r| r.ledger.used(None, Resource::HostBytes))
    }

    /// Returns currently active `ControlStore` reservations across this host instance.
    pub fn control_stores_used(&self) -> u64 {
        self.resources
            .as_ref()
            .map_or(0, |r| r.ledger.used(None, Resource::ControlStore))
    }

    /// Returns currently reserved `WasmBytes` across this host instance.
    pub fn wasm_bytes_used(&self) -> u64 {
        self.resources
            .as_ref()
            .map_or(0, |r| r.ledger.used(None, Resource::WasmBytes))
    }

    /// Returns `true` when all dynamic memory, temp-file, stream, relay, and track reservations are zero.
    pub fn is_ledger_empty(&self) -> bool {
        let Ok(resources) = &self.resources else {
            return true;
        };
        [
            Resource::HostBytes,
            Resource::TempBytes,
            Resource::PlaybackSession,
            Resource::RelayWorker,
            Resource::EnhancementTrack,
            Resource::Handle,
        ]
        .into_iter()
        .all(|res| resources.ledger.used(None, res) == 0)
    }

    pub fn open_window(
        &self,
        disabled: &[String],
        theme: &str,
    ) -> (WindowSession, UnboundedReceiver<()>) {
        let disabled_set: BTreeSet<String> = disabled.iter().cloned().collect();
        let opened = match &self.resources {
            Ok(resources) => {
                let mut slot = self.hub.lock().unwrap_or_else(|e| e.into_inner());
                let mut attempt = slot.as_ref().map(|hub| {
                    let _ = hub.sync_disabled(disabled_set.clone());
                    hub.open_window(Generation::default(), None)
                });
                if matches!(attempt, Some(Err(ErrorCode::Cancelled))) {
                    *slot = None;
                    attempt = None;
                }
                match attempt {
                    Some(result) => result,
                    None => match make_hub(resources, disabled_set) {
                        Ok(hub) => {
                            let opened = hub.open_window(Generation::default(), None);
                            *slot = Some(hub);
                            opened
                        }
                        Err(error) => Err(error),
                    },
                }
            }
            Err(error) => Err(*error),
        };
        let (client, wake) = opened.unwrap_or_else(Client::unavailable);
        (
            WindowSession::new(client, theme.strip_prefix("plugin:").map(str::to_owned)),
            wake,
        )
    }

    /// Reserves a bounded host annotation task against this host's `Ledger`.
    pub fn begin_host_annotation(&self) -> Result<HostAnnotationJob, ErrorCode> {
        let resources = self.resources()?;
        HostAnnotationJob::new(resources.ledger)
    }

    /// Signals coordinator shutdown, drains pending artifact deletions within
    /// bounded timeouts, and releases the host connection handle.
    pub fn shutdown(&self) {
        let _ = self.hub.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Ok(resources) = &self.resources {
            resources.shutdown();
        }
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn make_hub(resources: &Resources, disabled: BTreeSet<String>) -> Result<Hub, ErrorCode> {
    let root = iris_core::fs_util::data_dir().ok_or(ErrorCode::IoError)?;
    let packages = plugins_dir().ok_or(ErrorCode::IoError)?;
    make_hub_in(
        resources,
        root,
        packages,
        iris_core::vault::CredentialVault::system(),
        disabled,
        true,
    )
}

fn make_hub_in(
    resources: &Resources,
    root: std::path::PathBuf,
    packages: std::path::PathBuf,
    vault: iris_core::vault::CredentialVault,
    disabled: BTreeSet<String>,
    include_bundled: bool,
) -> Result<Hub, ErrorCode> {
    let (installer, bootstrap) = if include_bundled {
        let bootstrap = super::official::Bootstrap::embedded(root.clone())?;
        let installer =
            Installer::with_bundled(packages, resources.ledger.clone(), bootstrap.pins());
        (installer, Some(bootstrap))
    } else {
        (Installer::new(packages, resources.ledger.clone()), None)
    };
    Hub::new(Config {
        installer,
        bootstrap,
        runtime: Setup {
            resources: resources.clone(),
            state_directory: root.join("plugin-v2-state"),
            temporary_directory: root.join("plugin-v2-temp"),
            vault,
            wake: Arc::new(|| {}),
        },
        disabled,
    })
}

pub fn plugins_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("IRIS_PLUGINS_V2")
        .map(std::path::PathBuf::from)
        .or_else(|| iris_core::fs_util::data_dir().map(|root| root.join("plugins-v2")))
}
