//! Control-plane & vector-overlay plugin manager (`PROTOCOL_VERSION = 1`).
//!
//! Manages built-in official plugins (`iris.guides`, `iris.ab-loop`, `iris.webdav`),
//! declarative theme packs, and external out-of-process plugins discovered under
//! `<app_data_dir>/plugins/<id>/plugin.json` (or `IRIS_PLUGINS`).

mod builtins;
mod process;
mod slots;
mod snapshot;
mod types;
mod webdav;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use futures::channel::mpsc::UnboundedReceiver;
use iris_api::{
    BuiltinPlugin, ChromeLayoutSpec, CommandSpec, HostCommand, HostEvent, OverlayNode, Permission,
    PlaybackSnapshot, PluginModal, SidebarPanel, SourceDescriptor, SourceProviderSpec, StatusBadge,
    ToolbarButton, UiThemeSpec,
};

use crate::vault::CredentialVault;

use builtins::{AbLoopPlugin, GuidesPlugin};
use process::{MAX_DRAIN_PER_POLL, WakeSignal};
use slots::{
    LoadedPlugin, Runtime, apply_plugin_commands, default_plugin_state_dir, merge_chrome,
    merge_theme,
};
use snapshot::sync_cached_snapshot;
use types::{
    RESERVED_NAMESPACE_ERR, discover_external_manifests, dispatch_to_loaded, start_loaded_plugin,
};
use webdav::WebDavPlugin;

pub use slots::DirtySlots;
pub use snapshot::snapshot_from_session;
pub use types::{PluginInfo, PluginSource, PluginStatus, PollReport, plugins_dir};

pub struct PluginManager {
    plugins: BTreeMap<String, LoadedPlugin>,
    host_version: String,
    wake: Option<WakeSignal>,
    builtin_tx: Sender<(String, Vec<HostCommand>)>,
    builtin_rx: Receiver<(String, Vec<HostCommand>)>,
    vault: CredentialVault,
    state_dir: Option<PathBuf>,
    external_dir_override: Option<Option<PathBuf>>,
    last_snapshot: PlaybackSnapshot,
}

impl PluginManager {
    fn new_empty(
        state_dir: Option<PathBuf>,
        external_dir_override: Option<Option<PathBuf>>,
        vault: CredentialVault,
    ) -> (Self, UnboundedReceiver<()>) {
        let (wake, rx) = WakeSignal::new();
        let (builtin_tx, builtin_rx) = mpsc::channel();
        (
            Self {
                plugins: BTreeMap::new(),
                host_version: env!("CARGO_PKG_VERSION").to_string(),
                wake: Some(wake),
                builtin_tx,
                builtin_rx,
                vault,
                state_dir,
                external_dir_override,
                last_snapshot: PlaybackSnapshot::default(),
            },
            rx,
        )
    }

    pub fn load_with_wake(disabled: &[String]) -> (Self, UnboundedReceiver<()>) {
        Self::load_with_wake_and_vault(disabled, CredentialVault::system())
    }

    pub fn load_with_wake_and_vault(
        disabled: &[String],
        vault: CredentialVault,
    ) -> (Self, UnboundedReceiver<()>) {
        let (mut mgr, rx) = Self::new_empty(default_plugin_state_dir(), None, vault);
        let _ = mgr.reload(disabled);
        (mgr, rx)
    }

    pub fn load_with_dirs_and_vault(
        external_dir: Option<&Path>,
        state_dir: Option<PathBuf>,
        disabled: &[String],
        vault: CredentialVault,
    ) -> (Self, UnboundedReceiver<()>) {
        let (mut mgr, rx) = Self::new_empty(state_dir, Some(external_dir.map(Into::into)), vault);
        let _ = mgr.reload_from(external_dir, disabled);
        (mgr, rx)
    }

    pub fn with_dirs_and_vault(
        external_dir: Option<&Path>,
        state_dir: Option<PathBuf>,
        disabled: &[String],
        vault: CredentialVault,
    ) -> Self {
        Self::load_with_dirs_and_vault(external_dir, state_dir, disabled, vault).0
    }

    pub fn reload(&mut self, disabled: &[String]) -> PollReport {
        self.reload_with_snapshot(disabled, self.last_snapshot.clone())
    }

    pub fn reload_with_snapshot(
        &mut self,
        disabled: &[String],
        snapshot: PlaybackSnapshot,
    ) -> PollReport {
        let dir = self
            .external_dir_override
            .clone()
            .unwrap_or_else(plugins_dir);
        self.reload_from_with_snapshot(dir.as_deref(), disabled, snapshot)
    }

    pub fn reload_from(&mut self, external_dir: Option<&Path>, disabled: &[String]) -> PollReport {
        self.reload_from_with_snapshot(external_dir, disabled, self.last_snapshot.clone())
    }

    pub fn reload_from_with_snapshot(
        &mut self,
        external_dir: Option<&Path>,
        disabled: &[String],
        snapshot: PlaybackSnapshot,
    ) -> PollReport {
        self.last_snapshot = snapshot.clone();
        if self.external_dir_override.is_some() {
            self.external_dir_override = Some(external_dir.map(Path::to_path_buf));
        }
        for p in self.plugins.values_mut() {
            p.stop_runtime();
        }
        self.plugins.clear();
        while self.builtin_rx.try_recv().is_ok() {}

        let mut report = PollReport {
            commands: Vec::new(),
            dirty: DirtySlots::all(),
            status_changed: true,
        };
        let builtins: [Box<dyn BuiltinPlugin>; 3] = [
            Box::new(GuidesPlugin::default()),
            Box::new(AbLoopPlugin::default()),
            Box::new(WebDavPlugin::default()),
        ];
        for b in builtins {
            let manifest = b.manifest();
            let id = manifest.id.clone();
            let enabled = !disabled.iter().any(|d| d == &id);
            let mut loaded = LoadedPlugin::new(
                self.state_dir.as_deref(),
                manifest,
                PluginSource::Builtin,
                enabled,
                Runtime::Builtin(b),
            );
            if enabled {
                report
                    .commands
                    .extend(self.start_loaded(&snapshot, &mut loaded).commands);
            }
            self.plugins.insert(id, loaded);
        }

        for (pdir, manifest) in external_dir.map_or_else(Vec::new, discover_external_manifests) {
            let id = manifest.id.clone();
            if self.plugins.contains_key(&id) {
                log::warn!("ignoring external plugin shadowing existing id '{id}'");
                continue;
            }
            let enabled = !disabled.iter().any(|d| d == &id);
            let runtime = if manifest.entry.is_none() {
                Runtime::Declarative
            } else {
                Runtime::External {
                    dir: pdir.clone(),
                    proc: None,
                }
            };
            let mut loaded = LoadedPlugin::new(
                self.state_dir.as_deref(),
                manifest,
                PluginSource::External(pdir),
                enabled,
                runtime,
            );
            if id.starts_with("iris.") {
                log::warn!("rejected external plugin '{id}': {RESERVED_NAMESPACE_ERR}");
                loaded.status = PluginStatus::Error(RESERVED_NAMESPACE_ERR.into());
            } else if enabled {
                report
                    .commands
                    .extend(self.start_loaded(&snapshot, &mut loaded).commands);
            }
            self.plugins.insert(id, loaded);
        }
        report
    }

    fn start_loaded(&self, snapshot: &PlaybackSnapshot, loaded: &mut LoadedPlugin) -> PollReport {
        start_loaded_plugin(
            &self.host_version,
            self.wake.clone(),
            &self.builtin_tx,
            &self.vault,
            self.state_dir.as_deref(),
            snapshot,
            loaded,
        )
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> PollReport {
        self.set_enabled_with_snapshot(id, enabled, self.last_snapshot.clone())
    }

    pub fn set_enabled_with_snapshot(
        &mut self,
        id: &str,
        enabled: bool,
        snapshot: PlaybackSnapshot,
    ) -> PollReport {
        self.last_snapshot = snapshot.clone();
        let Some(mut loaded) = self.plugins.remove(id) else {
            return PollReport::default();
        };
        if loaded.enabled == enabled {
            self.plugins.insert(id.to_string(), loaded);
            return PollReport::default();
        }
        let report = if !enabled {
            loaded.stop_runtime();
            loaded.enabled = false;
            loaded.status = PluginStatus::Disabled;
            PollReport {
                dirty: loaded.clear_slots(),
                status_changed: true,
                ..Default::default()
            }
        } else {
            loaded.enabled = true;
            self.start_loaded(&snapshot, &mut loaded)
        };
        self.plugins.insert(id.to_string(), loaded);
        report
    }

    /// Returns whether at least one running plugin holds [`Permission::ReadState`]
    /// (allows the UI loop to skip 5Hz `Tick` dispatch when no listener is active).
    pub fn has_state_listeners(&self) -> bool {
        self.plugins
            .values()
            .any(|p| p.is_running() && p.manifest.has_permission(Permission::ReadState))
    }

    /// Dynamic connection creation providers exposed by running `MediaSource` plugins.
    pub fn source_providers(&self) -> Vec<(String, SourceProviderSpec)> {
        self.plugins
            .values()
            .filter(|p| p.is_running() && p.manifest.has_permission(Permission::MediaSource))
            .filter_map(|p| {
                p.manifest
                    .source_provider
                    .as_ref()
                    .map(|s| (p.manifest.id.clone(), s.clone()))
            })
            .collect()
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .values()
            .map(|p| PluginInfo {
                manifest: p.manifest.clone(),
                source: p.source.clone(),
                status: p.status.clone(),
                enabled: p.enabled,
            })
            .collect()
    }

    pub fn commands(&self) -> Vec<(&str, &str, &CommandSpec)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .flat_map(|p| {
                p.manifest
                    .commands
                    .iter()
                    .map(move |c| (p.manifest.id.as_str(), p.manifest.name.as_str(), c))
            })
            .collect()
    }

    pub fn sidebar_panels(&self) -> Vec<(&str, &SidebarPanel)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .filter_map(|p| p.sidebar.as_ref().map(|s| (p.manifest.id.as_str(), s)))
            .collect()
    }

    pub fn status_badges(&self) -> Vec<(&str, &StatusBadge)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .filter_map(|p| p.status_badge.as_ref().map(|b| (p.manifest.id.as_str(), b)))
            .collect()
    }

    pub fn overlay_nodes(&self) -> Vec<(&str, &OverlayNode)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .flat_map(|p| p.overlay.iter().map(move |n| (p.manifest.id.as_str(), n)))
            .collect()
    }

    pub fn toolbar_buttons(&self) -> Vec<(&str, &ToolbarButton)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .flat_map(|p| p.toolbar.iter().map(move |b| (p.manifest.id.as_str(), b)))
            .collect()
    }

    pub fn active_modal(&self) -> Option<(&str, &PluginModal)> {
        self.plugins
            .values()
            .filter(|p| p.is_running())
            .find_map(|p| p.modal.as_ref().map(|m| (p.manifest.id.as_str(), m)))
    }

    pub fn dismiss_modal(&mut self, plugin_id: &str) -> DirtySlots {
        let mut dirty = DirtySlots::default();
        if let Some(loaded) = self.plugins.get_mut(plugin_id)
            && loaded.modal.take().is_some()
        {
            dirty.modal = true;
        }
        dirty
    }

    pub fn active_ui_overrides(&self) -> (Option<UiThemeSpec>, Option<ChromeLayoutSpec>) {
        let (mut merged_theme, mut merged_chrome) = (None, None);
        for p in self
            .plugins
            .values()
            .filter(|p| p.is_running() && p.manifest.has_permission(Permission::UiTheme))
        {
            if let Some(t) = &p.active_theme {
                merge_theme(merged_theme.get_or_insert_with(UiThemeSpec::default), t);
            }
            if let Some(c) = &p.active_chrome {
                merge_chrome(
                    merged_chrome.get_or_insert_with(ChromeLayoutSpec::default),
                    c,
                );
            }
        }
        (merged_theme, merged_chrome)
    }

    pub fn media_sources(&self) -> Vec<(&str, &str, &SourceDescriptor)> {
        self.plugins
            .values()
            .filter(|p| p.is_running() && p.manifest.has_permission(Permission::MediaSource))
            .flat_map(|p| {
                p.effective_sources()
                    .iter()
                    .map(move |s| (p.manifest.id.as_str(), p.manifest.name.as_str(), s))
            })
            .collect()
    }

    pub fn broadcast(&mut self, event: &HostEvent) -> PollReport {
        sync_cached_snapshot(&mut self.last_snapshot, event);
        let (mut report, mut encoded) = (PollReport::default(), None);
        let (vault, state_dir) = (&self.vault, self.state_dir.as_deref());
        for loaded in self.plugins.values_mut() {
            if loaded.is_running() && loaded.manifest.has_permission(Permission::ReadState) {
                dispatch_to_loaded(loaded, event, &mut encoded, vault, state_dir, &mut report);
            }
        }
        report
    }

    pub fn poll(&mut self) -> PollReport {
        if let Some(wake) = &self.wake {
            wake.clear();
        }
        let mut report = PollReport::default();
        let (vault, state_dir) = (&self.vault, self.state_dir.as_deref());

        for _ in 0..MAX_DRAIN_PER_POLL {
            match self.builtin_rx.try_recv() {
                Ok((pid, cmds)) => {
                    if let Some(loaded) = self.plugins.get_mut(&pid).filter(|l| l.is_running()) {
                        let (fwd, dirty) = apply_plugin_commands(loaded, vault, state_dir, cmds);
                        report.dirty.merge(dirty);
                        report
                            .commands
                            .extend(fwd.into_iter().map(|c| (pid.clone(), c)));
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        for (pid, loaded) in &mut self.plugins {
            if !loaded.enabled {
                continue;
            }
            let Runtime::External { proc: Some(p), .. } = &mut loaded.runtime else {
                continue;
            };
            let (emitted, exited) = p.drain();
            if exited.is_some()
                && let Runtime::External { proc, .. } = &mut loaded.runtime
            {
                *proc = None;
            }
            if !emitted.is_empty() {
                let (fwd, dirty) = apply_plugin_commands(loaded, vault, state_dir, emitted);
                report.dirty.merge(dirty);
                report
                    .commands
                    .extend(fwd.into_iter().map(|c| (pid.clone(), c)));
            }
            if let Some(reason) = exited {
                loaded.status = PluginStatus::Error(reason);
                report.dirty.merge(loaded.clear_slots());
                report.status_changed = true;
            }
        }
        report
    }

    pub fn send_to(&mut self, plugin_id: &str, event: HostEvent) -> PollReport {
        let (vault, state_dir) = (&self.vault, self.state_dir.as_deref());
        let Some(loaded) = self.plugins.get_mut(plugin_id).filter(|l| l.is_running()) else {
            return PollReport::default();
        };
        let mut report = PollReport::default();
        if let HostEvent::ModalCancel { id } = &event
            && loaded.modal.as_ref().is_some_and(|m| &m.id == id)
        {
            loaded.modal = None;
            report.dirty.modal = true;
        }
        let mut encoded = None;
        dispatch_to_loaded(loaded, &event, &mut encoded, vault, state_dir, &mut report);
        report
    }
}
