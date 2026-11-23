//! Host-owned UI slot state, value-equality dirty tracking (`DirtySlots`),
//! theme/chrome merging, and scoped state/secret persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use iris_api::{
    BuiltinPlugin, ChromeLayoutSpec, Color, HostCommand, HostEvent, OverlayNode, Permission,
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
    warnings_left: u8,
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
            warnings_left: 16,
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

// Count GPUI children too: a grid, histogram, or button can expand into many elements.
pub(super) fn overlay_elements(node: &OverlayNode) -> usize {
    match node {
        OverlayNode::Grid { cols, rows, .. } => {
            usize::from((*cols).clamp(2, 16) + (*rows).clamp(2, 16) - 2)
        }
        OverlayNode::Bars { values, .. } => 1 + values.len(),
        OverlayNode::Crosshair { .. } => 2,
        OverlayNode::Text { .. } | OverlayNode::Button { .. } => 4,
        OverlayNode::Line { .. } | OverlayNode::Box { .. } => 1,
    }
}

pub(super) fn overlay_fits(nodes: &[OverlayNode]) -> bool {
    fn color_fits(color: &Color) -> bool {
        !matches!(color, Color::Hex(s) if s.len() > 9)
    }

    if nodes.len() > 64 {
        return false;
    }
    let mut elements = 0;
    let mut text_bytes = 0;
    for node in nodes {
        let fits = match node {
            OverlayNode::Box { fill, border, .. } => fill.iter().chain(border).all(color_fits),
            OverlayNode::Line { color, .. }
            | OverlayNode::Crosshair { color, .. }
            | OverlayNode::Grid { color, .. } => color_fits(color),
            OverlayNode::Bars { values, color, .. } => values.len() <= 128 && color_fits(color),
            OverlayNode::Text {
                text, color, bg, ..
            } => {
                text_bytes += text.len();
                text.len() <= 4096 && color_fits(color) && bg.iter().all(color_fits)
            }
            OverlayNode::Button {
                id,
                action_id,
                icon,
                label,
                ..
            } => {
                text_bytes += label.len();
                id.len() <= 256
                    && action_id.len() <= 256
                    && icon.as_ref().is_none_or(|s| s.len() <= 256)
                    && label.len() <= 4096
            }
        };
        if !fits || text_bytes > 16 * 1024 {
            return false;
        }
        elements += overlay_elements(node);
        if elements > 512 {
            return false;
        }
    }
    true
}

// Keep payloads (including credential keys, values, and stream headers) out of refusal logs.
fn command_name(cmd: &HostCommand) -> &'static str {
    macro_rules! variants {
        ($($variant:ident),+ $(,)?) => {
            match cmd { $(HostCommand::$variant { .. } => stringify!($variant)),+ }
        };
    }
    variants!(
        TogglePlayback,
        SeekRelative,
        SeekAbsolute,
        SetSpeed,
        SetVolume,
        ToggleMute,
        OpenNext,
        OpenPrevious,
        ToggleFavorite,
        ShowToast,
        SetState,
        SetSecret,
        SetSidebar,
        SetStatus,
        SetOverlay,
        SetToolbar,
        OpenModal,
        CloseModal,
        SetTheme,
        SetSources,
        ProvideSourcePage,
        ResolveStream,
        SourceError,
    )
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
            if loaded.warnings_left > 0 {
                loaded.warnings_left -= 1;
                log::warn!(
                    "plugin {} denied {} (missing {:?})",
                    loaded.manifest.id,
                    command_name(&cmd),
                    perm
                );
            }
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
                if vault.set_secret(&scope, &key, value.as_deref()).is_err()
                    && loaded.warnings_left > 0
                {
                    loaded.warnings_left -= 1;
                    log::warn!("plugin {} failed to store credential", loaded.manifest.id);
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
                if overlay_fits(&nodes) {
                    set_if_changed(&mut loaded.overlay, nodes, &mut dirty.overlay);
                } else if loaded.warnings_left > 0 {
                    loaded.warnings_left -= 1;
                    log::warn!("plugin {} exceeded overlay budget", loaded.manifest.id);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_labels_never_include_command_fields() {
        let command = HostCommand::SetSecret {
            key: "private key".into(),
            value: Some("private value".into()),
        };
        assert_eq!(command_name(&command), "SetSecret");
    }

    #[test]
    fn overlay_enforces_nested_and_expanded_limits_before_replacement() {
        let mut plugin = LoadedPlugin::new(
            None,
            PluginManifest {
                permissions: vec![Permission::UiOverlay],
                ..Default::default()
            },
            PluginSource::Builtin,
            true,
            Runtime::Declarative,
        );
        let vault = CredentialVault::in_memory();
        let small: OverlayNode =
            serde_json::from_value(serde_json::json!({"kind":"grid", "cols":2, "rows":2})).unwrap();
        let grid = OverlayNode::Grid {
            cols: 16,
            rows: 16,
            color: Color::White,
            opacity: 1.0,
        };
        let make_text = |text: String| {
            serde_json::from_value(
                serde_json::json!({"kind":"text", "text":text, "x":0.5, "y":0.5}),
            )
            .unwrap()
        };
        let bars = OverlayNode::Bars {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            values: vec![0.1; 129],
            color: Color::White,
            opacity: 0.5,
        };
        assert!(
            apply_plugin_commands(
                &mut plugin,
                &vault,
                None,
                vec![HostCommand::SetOverlay {
                    nodes: vec![small.clone()]
                }]
            )
            .1
            .overlay
        );
        for invalid in [
            vec![small.clone(); 65],
            vec![grid; 18],
            vec![bars],
            vec![make_text("字".repeat(1400))],
            vec![make_text("x".repeat(4096)); 5],
        ] {
            let (_, dirty) = apply_plugin_commands(
                &mut plugin,
                &vault,
                None,
                vec![HostCommand::SetOverlay { nodes: invalid }],
            );
            assert!(!dirty.overlay);
            assert_eq!(plugin.overlay.as_slice(), std::slice::from_ref(&small));
        }
        assert!(
            apply_plugin_commands(
                &mut plugin,
                &vault,
                None,
                vec![HostCommand::SetOverlay { nodes: Vec::new() }]
            )
            .1
            .overlay
        );
        assert!(plugin.overlay.is_empty());
    }
}
