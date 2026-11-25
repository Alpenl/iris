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
use std::sync::{Arc, OnceLock};

impl PluginManager {
    pub fn load_with_wake(disabled: &[String], theme: &str) -> (Self, UnboundedReceiver<()>) {
        static HUB: OnceLock<Result<Hub, ErrorCode>> = OnceLock::new();
        let opened = HUB
            .get_or_init(|| make_hub(disabled))
            .as_ref()
            .map_err(|e| *e)
            .and_then(|hub| hub.open_window(Generation::default(), None));
        let (client, wake) = opened.unwrap_or_else(Client::unavailable);
        (
            Self::new(client, theme.strip_prefix("plugin:").map(str::to_owned)),
            wake,
        )
    }
}

fn make_hub(disabled: &[String]) -> Result<Hub, ErrorCode> {
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
        disabled: disabled.iter().cloned().collect(),
    })
}

pub fn plugins_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("IRIS_PLUGINS_V2")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::fs_util::data_dir().map(|root| root.join("plugins-v2")))
}
