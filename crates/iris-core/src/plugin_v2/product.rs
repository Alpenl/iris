//! Product wiring. Windows share one coordinator; package discovery/compilation
//! remains on its workers. An unavailable plugin runtime never prevents native playback.
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

static HUB: Mutex<Option<Hub>> = Mutex::new(None);

impl PluginManager {
    pub fn load_with_wake(disabled: &[String], theme: &str) -> (Self, UnboundedReceiver<()>) {
        let disabled_set: BTreeSet<String> = disabled.iter().cloned().collect();
        let opened = {
            let mut slot = HUB.lock().unwrap_or_else(|e| e.into_inner());
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
                None => match make_hub(disabled_set) {
                    Ok(hub) => {
                        let opened = hub.open_window(Generation::default(), None);
                        *slot = Some(hub);
                        opened
                    }
                    Err(error) => Err(error),
                },
            }
        };
        let (client, wake) = opened.unwrap_or_else(Client::unavailable);
        (
            Self::new(client, theme.strip_prefix("plugin:").map(str::to_owned)),
            wake,
        )
    }
}

/// Releases the process-wide plugin coordinator so app exit or tests can cleanly re-initialize.
pub fn shutdown_hub() {
    let _ = HUB.lock().unwrap_or_else(|e| e.into_inner()).take();
}

fn make_hub(disabled: BTreeSet<String>) -> Result<Hub, ErrorCode> {
    let root = crate::fs_util::data_dir().ok_or(ErrorCode::IoError)?;
    let resources = Resources::global()?;
    let packages = plugins_dir().ok_or(ErrorCode::IoError)?;
    let bootstrap = super::official::Bootstrap::embedded(root.clone())?;
    Hub::new(Config {
        installer: Installer::with_bundled(packages, resources.ledger.clone(), bootstrap.pins()),
        bootstrap: Some(bootstrap),
        runtime: Setup {
            resources,
            state_directory: root.join("plugin-v2-state"),
            temporary_directory: root.join("plugin-v2-temp"),
            vault: crate::vault::CredentialVault::system(),
            wake: Arc::new(|| {}),
        },
        disabled,
    })
}

pub fn plugins_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("IRIS_PLUGINS_V2")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::fs_util::data_dir().map(|root| root.join("plugins-v2")))
}
