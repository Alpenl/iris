//! Explicit application-owned [`PluginHost`] context. Package discovery and
//! Wasm compilation run on background coordinator workers; an unavailable
//! plugin runtime never prevents native playback.
use super::{
    hub::{Client, Config, Hub},
    installation::Installer,
    runtime::{Resources, Setup},
    ui::PluginManager,
};
use futures::channel::mpsc::UnboundedReceiver;
use iris_api::v2::{ErrorCode, Generation};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// Explicit host runtime context owned by the application or an isolated test.
/// Dropping or calling [`PluginHost::shutdown`] tears down the coordinator without
/// process-wide mutable statics.
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

    /// Wraps caller-provided `Resources` and `Hub` for deterministic isolated tests.
    pub fn from_parts(resources: Resources, hub: Hub) -> Self {
        Self {
            resources: Ok(resources),
            hub: Mutex::new(Some(hub)),
        }
    }

    pub fn resources(&self) -> Result<Resources, ErrorCode> {
        self.resources.clone()
    }

    pub fn open_manager(
        &self,
        disabled: &[String],
        theme: &str,
    ) -> (PluginManager, UnboundedReceiver<()>) {
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
            PluginManager::new(client, theme.strip_prefix("plugin:").map(str::to_owned)),
            wake,
        )
    }

    /// Signals coordinator shutdown and releases the host connection handle.
    pub fn shutdown(&self) {
        let _ = self.hub.lock().unwrap_or_else(|e| e.into_inner()).take();
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
    let bootstrap = super::official::Bootstrap::embedded(root.clone())?;
    Hub::new(Config {
        installer: Installer::with_bundled(packages, resources.ledger.clone(), bootstrap.pins()),
        bootstrap: Some(bootstrap),
        runtime: Setup {
            resources: resources.clone(),
            state_directory: root.join("plugin-v2-state"),
            temporary_directory: root.join("plugin-v2-temp"),
            vault: iris_core::vault::CredentialVault::system(),
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
