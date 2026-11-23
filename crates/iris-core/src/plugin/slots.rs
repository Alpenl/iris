//! Host-owned UI slot state, value-equality dirty tracking (`DirtySlots`),
//! theme/chrome merging, and scoped state/secret persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use iris_api::{
    BuiltinPlugin, ChromeLayoutSpec, HostCommand, HostEvent, OverlayNode, Permission,
    PluginManifest, PluginModal, SidebarPanel, SourceDescriptor, StatusBadge, ToolbarButton,
    UiThemeSpec, is_valid_id,
};

use crate::fs_util;
use crate::vault::CredentialVault;

use super::process::ExternalProcess;
use super::{PluginSource, PluginStatus};

/// Bitset-style record of host UI slots whose cached values genuinely changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtySlots {
    pub sidebar: bool,
    pub status: bool,
    pub toolbar: bool,
    pub overlay: bool,
    pub theme: bool,
    pub modal: bool,
    pub sources: bool,
}

impl DirtySlots {
    pub const fn all() -> Self {
        Self {
            sidebar: true,
            status: true,
            toolbar: true,
            overlay: true,
            theme: true,
            modal: true,
            sources: true,
        }
    }

    pub const fn is_empty(self) -> bool {
        !(self.sidebar
            || self.status
            || self.toolbar
            || self.overlay
            || self.theme
            || self.modal
            || self.sources)
    }

    pub fn merge(&mut self, o: Self) {
        self.sidebar |= o.sidebar;
        self.status |= o.status;
        self.toolbar |= o.toolbar;
        self.overlay |= o.overlay;
        self.theme |= o.theme;
        self.modal |= o.modal;
        self.sources |= o.sources;
    }

    /// In immersive fullscreen (`fullscreen && !chrome_visible && !sidebar_visible`),
    /// only viewport-visible changes (`overlay`, `modal`, `theme`) warrant waking UI.
    pub const fn should_notify_ui(
        self,
        fullscreen: bool,
        chrome_visible: bool,
        sidebar_visible: bool,
    ) -> bool {
        if fullscreen && !chrome_visible && !sidebar_visible {
            self.overlay || self.modal || self.theme
        } else {
            !self.is_empty()
        }
    }
}

pub(super) enum Runtime {
    Builtin(Box<dyn BuiltinPlugin>),
    Declarative,
    External {
        dir: PathBuf,
        proc: Option<ExternalProcess>,
    },
}

pub(super) struct LoadedPlugin {
    pub(super) manifest: PluginManifest,
    pub(super) source: PluginSource,
    pub(super) status: PluginStatus,
    pub(super) enabled: bool,
    pub(super) runtime: Runtime,
    pub(super) state: BTreeMap<String, String>,
    pub(super) dynamic_sources: Option<Vec<SourceDescriptor>>,
    pub(super) sidebar: Option<SidebarPanel>,
    pub(super) status_badge: Option<StatusBadge>,
    pub(super) overlay: Vec<OverlayNode>,
    pub(super) toolbar: Vec<ToolbarButton>,
    pub(super) modal: Option<PluginModal>,
    pub(super) active_theme: Option<UiThemeSpec>,
    pub(super) active_chrome: Option<ChromeLayoutSpec>,
}

impl LoadedPlugin {
    pub(super) fn new(
        state_dir: Option<&Path>,
        manifest: PluginManifest,
        source: PluginSource,
        enabled: bool,
        runtime: Runtime,
    ) -> Self {
        let state = load_plugin_state(state_dir, &manifest.id);
        let status = if enabled {
            PluginStatus::Running
        } else {
            PluginStatus::Disabled
        };
        Self {
            manifest,
            source,
            status,
            enabled,
            runtime,
            state,
            dynamic_sources: None,
            sidebar: None,
            status_badge: None,
            overlay: Vec::new(),
            toolbar: Vec::new(),
            modal: None,
            active_theme: None,
            active_chrome: None,
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.enabled && matches!(self.status, PluginStatus::Running)
    }

    pub(super) fn effective_sources(&self) -> &[SourceDescriptor] {
        self.dynamic_sources
            .as_deref()
            .unwrap_or(&self.manifest.sources)
    }

    pub(super) fn stop_runtime(&mut self) {
        match &mut self.runtime {
            Runtime::External { proc, .. } => {
                proc.take();
            }
            Runtime::Builtin(b) if self.enabled => {
                let _ = b.handle_event(&HostEvent::Shutdown);
            }
            _ => {}
        }
    }

    pub(super) fn seed_manifest_theme(&mut self) -> DirtySlots {
        let mut dirty = DirtySlots::default();
        if self.manifest.has_permission(Permission::UiTheme)
            && (self.active_theme != self.manifest.theme
                || self.active_chrome != self.manifest.chrome)
        {
            self.active_theme = self.manifest.theme.clone();
            self.active_chrome = self.manifest.chrome.clone();
            dirty.theme = true;
        }
        if self.manifest.has_permission(Permission::MediaSource)
            && !self.manifest.sources.is_empty()
        {
            dirty.sources = true;
        }
        dirty
    }

    pub(super) fn clear_slots(&mut self) -> DirtySlots {
        let had_sources = self.manifest.has_permission(Permission::MediaSource)
            && !self.effective_sources().is_empty();
        let dirty = DirtySlots {
            sidebar: self.sidebar.take().is_some(),
            status: self.status_badge.take().is_some(),
            toolbar: !std::mem::take(&mut self.toolbar).is_empty(),
            overlay: !std::mem::take(&mut self.overlay).is_empty(),
            theme: self.active_theme.take().is_some() || self.active_chrome.take().is_some(),
            modal: self.modal.take().is_some(),
            sources: had_sources,
        };
        self.dynamic_sources = None;
        dirty
    }
}

pub(super) fn default_plugin_state_dir() -> Option<PathBuf> {
    fs_util::data_dir().map(|d| d.join("plugin_state"))
}

fn plugin_state_path_in(state_dir: Option<&Path>, plugin_id: &str) -> Option<PathBuf> {
    is_valid_id(plugin_id).then(|| state_dir.map(|d| d.join(format!("{plugin_id}.json"))))?
}

pub(super) fn load_plugin_state(state_dir: Option<&Path>, id: &str) -> BTreeMap<String, String> {
    plugin_state_path_in(state_dir, id)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub(super) fn save_plugin_state(
    state_dir: Option<&Path>,
    plugin_id: &str,
    state: &BTreeMap<String, String>,
) {
    let Some(path) = plugin_state_path_in(state_dir, plugin_id) else {
        return;
    };
    if state.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(state)
        && let Err(e) = fs_util::atomic_write(&path, json.as_bytes())
    {
        log::warn!("failed saving plugin state for {plugin_id}: {e}");
    }
}

pub(super) fn secret_scope(source: &PluginSource, plugin_id: &str) -> String {
    match source {
        PluginSource::Builtin => format!("builtin.{plugin_id}"),
        PluginSource::External(_) => format!("ext.{plugin_id}"),
    }
}

pub(super) fn load_plugin_secrets(
    vault: &CredentialVault,
    source: &PluginSource,
    manifest: &PluginManifest,
) -> BTreeMap<String, String> {
    if manifest.has_permission(Permission::Secrets) {
        vault.list_scope(&secret_scope(source, &manifest.id))
    } else {
        BTreeMap::new()
    }
}

fn set_if_changed<T: PartialEq>(slot: &mut T, next: T, flag: &mut bool) {
    if *slot != next {
        *slot = next;
        *flag = true;
    }
}

pub(super) fn apply_plugin_commands(
    loaded: &mut LoadedPlugin,
    vault: &CredentialVault,
    state_dir: Option<&Path>,
    commands: Vec<HostCommand>,
) -> (Vec<HostCommand>, DirtySlots) {
    let (mut forward, mut dirty) = (Vec::new(), DirtySlots::default());
    for cmd in commands {
        if let Some(perm) = cmd.required_permission()
            && !loaded.manifest.has_permission(perm)
        {
            log::warn!(
                "plugin {} denied {:?} (missing {:?})",
                loaded.manifest.id,
                cmd,
                perm
            );
            continue;
        }
        match cmd {
            HostCommand::SetState { key, value } => {
                let k = key.trim();
                if !k.is_empty() {
                    let changed = match value {
                        Some(v) => {
                            loaded.state.insert(k.to_string(), v.clone()).as_ref() != Some(&v)
                        }
                        None => loaded.state.remove(k).is_some(),
                    };
                    if changed {
                        save_plugin_state(state_dir, &loaded.manifest.id, &loaded.state);
                    }
                }
            }
            HostCommand::SetSecret { key, value } => {
                let scope = secret_scope(&loaded.source, &loaded.manifest.id);
                if let Err(e) = vault.set_secret(&scope, &key, value.as_deref()) {
                    log::warn!("plugin {} failed secret '{}': {e}", loaded.manifest.id, key);
                }
            }
            HostCommand::SetSources { sources } => {
                dirty.sources |= loaded.effective_sources() != sources.as_slice();
                loaded.dynamic_sources = Some(sources);
            }
            HostCommand::SetSidebar { panel } => {
                set_if_changed(&mut loaded.sidebar, panel.map(|b| *b), &mut dirty.sidebar);
            }
            HostCommand::SetStatus { badge } => {
                set_if_changed(
                    &mut loaded.status_badge,
                    badge.map(|b| *b),
                    &mut dirty.status,
                );
            }
            HostCommand::SetOverlay { nodes } => {
                set_if_changed(&mut loaded.overlay, nodes, &mut dirty.overlay);
            }
            HostCommand::SetToolbar { buttons } => {
                set_if_changed(&mut loaded.toolbar, buttons, &mut dirty.toolbar);
            }
            HostCommand::OpenModal { modal } => {
                set_if_changed(&mut loaded.modal, Some(*modal), &mut dirty.modal);
            }
            HostCommand::CloseModal { id } => {
                if id
                    .as_deref()
                    .is_none_or(|t| loaded.modal.as_ref().is_some_and(|m| m.id == t))
                    && loaded.modal.take().is_some()
                {
                    dirty.modal = true;
                }
            }
            HostCommand::SetTheme { theme, chrome } => {
                let next_t = theme.map(|b| *b);
                if loaded.active_theme != next_t || loaded.active_chrome != chrome {
                    loaded.active_theme = next_t;
                    loaded.active_chrome = chrome;
                    dirty.theme = true;
                }
            }
            other => forward.push(other),
        }
    }
    (forward, dirty)
}

pub(super) fn merge_theme(dst: &mut UiThemeSpec, src: &UiThemeSpec) {
    macro_rules! cp { ($($f:ident),+) => { $(if let Some(v) = &src.$f { dst.$f = Some(v.clone()); })+ }; }
    cp!(
        name,
        window_bg,
        panel,
        hover,
        selected,
        line,
        line_strong,
        text,
        text_muted
    );
    cp!(
        text_dim,
        text_faint,
        accent,
        overlay,
        scrim,
        titlebar_height,
        dock_height,
        row_height,
        corner_radius
    );
}

pub(super) fn merge_chrome(dst: &mut ChromeLayoutSpec, src: &ChromeLayoutSpec) {
    macro_rules! cp { ($($f:ident),+) => { $(if let Some(v) = src.$f { dst.$f = Some(v); })+ }; }
    cp!(
        hide_titlebar,
        floating_dock,
        show_titlebar_meta,
        compact_controls
    );
    for btn in &src.hidden_dock_buttons {
        if !dst.hidden_dock_buttons.contains(btn) {
            dst.hidden_dock_buttons.push(btn.clone());
        }
    }
}
