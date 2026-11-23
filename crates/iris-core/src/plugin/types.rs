//! Public plugin status, metadata, [`PollReport`] types, and plugin start/dispatch helpers.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Weak};

use iris_api::{
    HostCommand, HostEvent, PROTOCOL_VERSION, Permission, PlaybackSnapshot, PluginManifest,
};

use crate::fs_util;
use crate::vault::CredentialVault;

use super::process::{ExternalProcess, WakeSignal, encode_event};
use super::slots::{
    DirtySlots, LoadedPlugin, Runtime, apply_plugin_commands, load_plugin_secrets,
    load_plugin_state, save_plugin_state,
};

pub(super) const RESERVED_NAMESPACE_ERR: &str = "reserved 'iris.*' namespace";

pub(super) type BuiltinReply = (String, Weak<()>, Vec<HostCommand>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginSource {
    Builtin,
    External(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    Running,
    Disabled,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub source: PluginSource,
    pub status: PluginStatus,
    pub enabled: bool,
}

/// Aggregated result of a [`super::PluginManager`] poll, event dispatch, or lifecycle mutation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PollReport {
    pub commands: Vec<(String, HostCommand)>,
    pub dirty: DirtySlots,
    pub status_changed: bool,
}

impl PollReport {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.dirty.is_empty() && !self.status_changed
    }

    pub fn merge(&mut self, other: Self) {
        self.commands.extend(other.commands);
        self.dirty.merge(other.dirty);
        self.status_changed |= other.status_changed;
    }

    pub fn should_notify_ui(
        &self,
        fullscreen: bool,
        chrome_visible: bool,
        sidebar_visible: bool,
    ) -> bool {
        !self.commands.is_empty()
            || self.status_changed
            || self
                .dirty
                .should_notify_ui(fullscreen, chrome_visible, sidebar_visible)
    }
}

impl From<PollReport> for (Vec<(String, HostCommand)>, DirtySlots) {
    fn from(r: PollReport) -> Self {
        (r.commands, r.dirty)
    }
}

/// Returns the user plugins folder (`IRIS_PLUGINS` override or `<data_dir>/plugins`).
pub fn plugins_dir() -> Option<PathBuf> {
    std::env::var_os("IRIS_PLUGINS")
        .map(PathBuf::from)
        .or_else(|| fs_util::data_dir().map(|d| d.join("plugins")))
}

pub(super) fn discover_external_manifests(dir: &Path) -> Vec<(PathBuf, PluginManifest)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in read.flatten() {
        let pdir = entry.path();
        if !pdir.is_dir() {
            continue;
        }
        let mpath = pdir.join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&mpath) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<PluginManifest>(&text) else {
            log::warn!("invalid plugin manifest: {}", mpath.display());
            continue;
        };
        if let Err(reason) = manifest.validate() {
            log::warn!("rejected plugin {}: {reason}", mpath.display());
            continue;
        }
        if manifest.entry.is_none() && manifest.theme.is_none() && manifest.chrome.is_none() {
            log::warn!("rejected plugin {}: missing entry/theme", mpath.display());
            continue;
        }
        found.push((pdir, manifest));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    found
}

pub(super) fn start_loaded_plugin(
    host_version: &str,
    wake: Option<WakeSignal>,
    builtin_tx: &Sender<BuiltinReply>,
    vault: &CredentialVault,
    state_dir: Option<&Path>,
    snapshot: &PlaybackSnapshot,
    loaded: &mut LoadedPlugin,
) -> PollReport {
    if matches!(loaded.source, PluginSource::External(_)) && loaded.manifest.id.starts_with("iris.")
    {
        loaded.status = PluginStatus::Error(RESERVED_NAMESPACE_ERR.into());
        return PollReport {
            dirty: loaded.clear_slots(),
            status_changed: true,
            ..Default::default()
        };
    }
    let mut dirty = loaded.seed_manifest_theme();
    let mut state = loaded
        .state_writer
        .as_ref()
        .and_then(|writer| writer.pending(&loaded.manifest.id))
        .unwrap_or_else(|| Arc::new(load_plugin_state(state_dir, &loaded.manifest.id)));
    if matches!(loaded.source, PluginSource::Builtin)
        && loaded.manifest.id == "iris.webdav"
        && super::webdav::reconcile_state(Arc::make_mut(&mut state), vault)
    {
        save_plugin_state(state_dir, &loaded.manifest.id, &state);
    }
    loaded.state = Arc::clone(&state);
    let secrets = load_plugin_secrets(vault, &loaded.source, &loaded.manifest);
    let init = HostEvent::Init {
        protocol: PROTOCOL_VERSION,
        host_version: host_version.to_string(),
        state: (*state).clone(),
        secrets,
        snapshot: if loaded.manifest.has_permission(Permission::ReadState) {
            snapshot.clone()
        } else {
            PlaybackSnapshot::default()
        },
    };
    let pid = loaded.manifest.id.clone();
    match &mut loaded.runtime {
        Runtime::Builtin(b) => {
            let (tx, wake_cb, cb_id) = (builtin_tx.clone(), wake, pid.clone());
            let generation = Arc::new(());
            let token = Arc::downgrade(&generation);
            loaded.builtin_generation = Some(generation);
            b.bind_responder(Arc::new(move |cmds| {
                if !cmds.is_empty()
                    && token.strong_count() != 0
                    && tx.send((cb_id.clone(), token.clone(), cmds)).is_ok()
                    && let Some(w) = &wake_cb
                {
                    w.notify();
                }
            }));
            loaded.status = PluginStatus::Running;
            let cmds = b.handle_event(&init);
            let (fwd, d) = apply_plugin_commands(loaded, vault, state_dir, cmds);
            dirty.merge(d);
            return PollReport {
                commands: fwd.into_iter().map(|c| (pid.clone(), c)).collect(),
                dirty,
                status_changed: true,
            };
        }
        Runtime::Declarative => loaded.status = PluginStatus::Running,
        Runtime::External { dir, proc } => {
            match ExternalProcess::spawn(dir, &loaded.manifest, wake)
                .and_then(|mut p| p.send(&init).map(|()| p))
            {
                Ok(p) => {
                    *proc = Some(p);
                    loaded.status = PluginStatus::Running;
                }
                Err(e) => {
                    loaded.status = PluginStatus::Error(e);
                    dirty.merge(loaded.clear_slots());
                }
            }
        }
    }
    PollReport {
        dirty,
        status_changed: true,
        ..Default::default()
    }
}

pub(super) fn dispatch_to_loaded(
    loaded: &mut LoadedPlugin,
    event: &HostEvent,
    cached_line: &mut Option<Arc<String>>,
    vault: &CredentialVault,
    state_dir: Option<&Path>,
    report: &mut PollReport,
) {
    let mut crashed = false;
    let emitted = match &mut loaded.runtime {
        Runtime::Builtin(b) => b.handle_event(event),
        Runtime::Declarative => Vec::new(),
        Runtime::External { proc, .. } => {
            let Some(p) = proc.as_mut() else {
                return;
            };
            let droppable = matches!(event, HostEvent::Tick { .. });
            let line = cached_line
                .clone()
                .map(Ok)
                .unwrap_or_else(|| encode_event(event).inspect(|l| *cached_line = Some(l.clone())));
            match line.and_then(|l| p.send_encoded(l, droppable)) {
                Ok(()) => {
                    let (cmds, exited) = p.drain();
                    if let Some(reason) = exited {
                        loaded.status = PluginStatus::Error(reason);
                        *proc = None;
                        crashed = true;
                    }
                    cmds
                }
                Err(e) => {
                    loaded.status = PluginStatus::Error(e);
                    *proc = None;
                    crashed = true;
                    Vec::new()
                }
            }
        }
    };
    let (fwd, dirty) = apply_plugin_commands(loaded, vault, state_dir, emitted);
    report.dirty.merge(dirty);
    if crashed {
        report.dirty.merge(loaded.clear_slots());
        report.status_changed = true;
    }
    let pid = loaded.manifest.id.clone();
    report
        .commands
        .extend(fwd.into_iter().map(|c| (pid.clone(), c)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_api::{BuiltinPlugin, Color, OverlayNode, Permission, PluginModal, ToolbarButton};

    struct RoguePlugin;
    impl BuiltinPlugin for RoguePlugin {
        fn manifest(&self) -> PluginManifest {
            PluginManifest {
                id: "test.rogue".into(),
                name: "Rogue".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                author: "Test".into(),
                description: String::new(),
                protocol: PROTOCOL_VERSION,
                permissions: vec![Permission::Notifications],
                ..Default::default()
            }
        }
        fn handle_event(&mut self, _: &HostEvent) -> Vec<HostCommand> {
            vec![
                HostCommand::TogglePlayback,
                HostCommand::SetOverlay {
                    nodes: vec![OverlayNode::Grid {
                        cols: 3,
                        rows: 3,
                        color: Color::White,
                        opacity: 0.5,
                    }],
                },
                HostCommand::ShowToast {
                    message: "allowed".into(),
                },
            ]
        }
    }

    #[test]
    fn permissions_block_unauthorized_commands_and_persist_baseline_state() {
        let dir = std::env::temp_dir().join(format!("iris-rogue-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = CredentialVault::in_memory();
        let mut loaded = LoadedPlugin::new(
            Some(&dir),
            RoguePlugin.manifest(),
            PluginSource::Builtin,
            true,
            Runtime::Builtin(Box::new(RoguePlugin)),
        );
        let mut emitted = RoguePlugin.handle_event(&HostEvent::Shutdown);
        emitted.push(HostCommand::SetToolbar {
            buttons: vec![ToolbarButton {
                id: "btn".into(),
                label: "X".into(),
                icon: None,
                tooltip: None,
                active: false,
                accent: false,
                action_id: "go".into(),
            }],
        });
        emitted.push(HostCommand::OpenModal {
            modal: Box::new(PluginModal {
                id: "m1".into(),
                title: "Auth".into(),
                subtitle: None,
                widgets: Vec::new(),
                submit_label: None,
                cancel_label: None,
            }),
        });
        emitted.push(HostCommand::SetSecret {
            key: "token".into(),
            value: Some("secret".into()),
        });
        emitted.push(HostCommand::SetState {
            key: "pref".into(),
            value: Some("dark".into()),
        });

        let (allowed, dirty) = apply_plugin_commands(&mut loaded, &vault, Some(&dir), emitted);
        assert_eq!(
            allowed,
            vec![HostCommand::ShowToast {
                message: "allowed".into()
            }]
        );
        assert!(dirty.is_empty());
        assert!(loaded.overlay.is_empty());
        assert!(loaded.toolbar.is_empty());
        assert!(loaded.modal.is_none());
        assert_eq!(vault.get_secret("builtin.test.rogue", "token"), None);
        assert!(dir.join("test.rogue.json").is_file());

        let _ = apply_plugin_commands(
            &mut loaded,
            &vault,
            Some(&dir),
            vec![HostCommand::SetState {
                key: "pref".into(),
                value: None,
            }],
        );
        assert!(!dir.join("test.rogue.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
