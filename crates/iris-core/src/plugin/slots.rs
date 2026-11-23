//! Host-owned UI slot state, value-equality dirty tracking (`DirtySlots`),
//! theme/chrome merging, and scoped state/secret persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use iris_api::Widget;
use iris_api::manifest::validate_sources;
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

    /// Redraw visible plugin slots, with hidden chrome suppressed in immersive fullscreen.
    pub const fn should_notify_ui(
        self,
        fullscreen: bool,
        chrome_visible: bool,
        sidebar_visible: bool,
    ) -> bool {
        if fullscreen && !chrome_visible && !sidebar_visible {
            self.overlay || self.modal || self.theme
        } else {
            self.overlay
                || self.modal
                || self.theme
                || self.sources
                || self.status
                || self.toolbar
                || (self.sidebar && sidebar_visible)
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
    pub(super) state: Arc<PluginState>,
    pub(super) state_writer: Option<StateWriter>,
    pub(super) builtin_generation: Option<Arc<()>>,
    pub(super) dynamic_sources: Option<Vec<SourceDescriptor>>,
    pub(super) sidebar: Option<SidebarPanel>,
    pub(super) status_badge: Option<StatusBadge>,
    pub(super) overlay: Vec<OverlayNode>,
    pub(super) toolbar: Vec<ToolbarButton>,
    pub(super) modal: Option<PluginModal>,
    pub(super) modal_order: u64,
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
            state: Arc::new(state),
            state_writer: None,
            builtin_generation: None,
            dynamic_sources: None,
            sidebar: None,
            status_badge: None,
            overlay: Vec::new(),
            toolbar: Vec::new(),
            modal: None,
            modal_order: 0,
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
        self.builtin_generation = None;
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

const MAX_STATE_BYTES: usize = 512 * 1024;
const MAX_UI_ELEMENTS: usize = 256;
const MAX_UI_TEXT_BYTES: usize = 64 * 1024;
static NEXT_MODAL_ORDER: AtomicU64 = AtomicU64::new(0);

fn json_string_bytes(s: &str) -> usize {
    2 + s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | 8 | 9 | 10 | 12 | 13 => 2,
            0..=31 => 6,
            _ => 1,
        })
        .sum::<usize>()
}

fn state_entry_bytes(key: &str, value: &str) -> usize {
    // Count both separators conservatively, including a trailing comma.
    json_string_bytes(key) + json_string_bytes(value) + 2
}

fn ui_fits<T: serde::Serialize>(value: &T, elements: usize) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(buf.len())
                .ok_or(std::io::ErrorKind::InvalidData)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    elements <= MAX_UI_ELEMENTS && serde_json::to_writer(Budget(MAX_UI_TEXT_BYTES), value).is_ok()
}

fn widget_elements(widget: &Widget) -> usize {
    1 + match widget {
        Widget::ButtonGroup { buttons } => buttons.len(),
        Widget::Select { options, .. } => options.len(),
        Widget::QrCode { matrix, .. } => {
            // The renderer expands the bounded matrix into rows of cells.
            let rows = matrix.iter().filter(|row| !row.is_empty()).take(96);
            let height = rows.clone().count();
            let width = rows
                .map(|row| row.chars().take(96).count())
                .max()
                .unwrap_or(0);
            height + (height * width).div_ceil(64)
        }
        _ => 0,
    }
}

pub(super) type PluginState = BTreeMap<String, String>;

#[derive(Default)]
struct StateWrites {
    pending: BTreeMap<String, Arc<PluginState>>,
    writing: Option<(String, Arc<PluginState>)>,
}

#[derive(Clone)]
pub(super) struct StateWriter {
    writes: Arc<(Mutex<StateWrites>, Condvar)>,
    wake: std::sync::mpsc::SyncSender<()>,
}

impl StateWriter {
    pub(super) fn new(dir: PathBuf) -> std::io::Result<Self> {
        let writes = Arc::new((Mutex::new(StateWrites::default()), Condvar::new()));
        let shared = Arc::clone(&writes);
        let (wake, work) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("iris-plugin-state".into())
            .spawn(move || {
                let (lock, flushed) = &*shared;
                while work.recv().is_ok() {
                    loop {
                        let item = {
                            let mut writes = lock.lock().unwrap();
                            let item = writes.pending.pop_first();
                            writes.writing = item.clone();
                            if item.is_none() {
                                flushed.notify_all();
                            }
                            item
                        };
                        let Some((id, state)) = item else {
                            break;
                        };
                        save_plugin_state(Some(&dir), &id, &state);
                    }
                }
            })?;
        Ok(Self { writes, wake })
    }

    fn enqueue(&self, plugin_id: &str, state: Arc<PluginState>) {
        self.writes
            .0
            .lock()
            .unwrap()
            .pending
            .insert(plugin_id.to_owned(), state);
        let _ = self.wake.try_send(());
    }

    pub(super) fn pending(&self, plugin_id: &str) -> Option<Arc<PluginState>> {
        let writes = self.writes.0.lock().unwrap();
        let state = writes.pending.get(plugin_id).or_else(|| {
            writes
                .writing
                .as_ref()
                .filter(|(id, _)| id == plugin_id)
                .map(|(_, state)| state)
        });
        state.cloned()
    }

    pub(super) fn wait_until(&self, deadline: Instant) {
        let (lock, flushed) = &*self.writes;
        drop(
            flushed
                .wait_timeout_while(
                    lock.lock().unwrap(),
                    deadline.saturating_duration_since(Instant::now()),
                    |writes| !writes.pending.is_empty() || writes.writing.is_some(),
                )
                .unwrap(),
        );
    }
}

pub(super) fn load_plugin_state(state_dir: Option<&Path>, id: &str) -> BTreeMap<String, String> {
    // Preserve pre-quota built-in WebDAV records so legacy endpoints can be scrubbed.
    let max_bytes = if id == "iris.webdav" {
        16 * 1024 * 1024
    } else {
        MAX_STATE_BYTES
    };
    plugin_state_path_in(state_dir, id)
        .and_then(|p| {
            use std::io::Read;
            let mut text = String::new();
            std::fs::File::open(p)
                .ok()?
                .take((max_bytes + 1) as u64)
                .read_to_string(&mut text)
                .ok()?;
            (text.len() <= max_bytes).then_some(text)
        })
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
    if let Ok(json) = serde_json::to_string(state)
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
        RefreshSource,
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
    let mut state_bytes = None;
    let mut state_changed = false;
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
                let key = key.trim();
                let old = loaded.state.get(key);
                if key.is_empty() || old == value.as_ref() {
                    continue;
                }
                let bytes = state_bytes.get_or_insert_with(|| {
                    2 + loaded
                        .state
                        .iter()
                        .map(|(key, value)| state_entry_bytes(key, value))
                        .sum::<usize>()
                });
                let next = *bytes - old.map_or(0, |old| state_entry_bytes(key, old))
                    + value
                        .as_ref()
                        .map_or(0, |value| state_entry_bytes(key, value));
                if next > MAX_STATE_BYTES && next >= *bytes {
                    continue;
                }
                *bytes = next;
                state_changed = true;
                let state = Arc::make_mut(&mut loaded.state);
                if let Some(value) = value {
                    state.insert(key.to_owned(), value);
                } else {
                    state.remove(key);
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
                if validate_sources(&sources).is_err() || !ui_fits(&sources, sources.len()) {
                    continue;
                }
                dirty.sources |= loaded.effective_sources() != sources.as_slice();
                loaded.dynamic_sources = Some(sources);
            }
            HostCommand::SetSidebar { panel } => {
                if let Some(panel) = &panel {
                    let elements = panel.sections.len()
                        + panel
                            .sections
                            .iter()
                            .flat_map(|section| &section.widgets)
                            .map(widget_elements)
                            .sum::<usize>();
                    if !ui_fits(panel, elements) {
                        continue;
                    }
                }
                set_if_changed(&mut loaded.sidebar, panel.map(|b| *b), &mut dirty.sidebar);
            }
            HostCommand::SetStatus { badge } => {
                if badge.as_ref().is_some_and(|badge| !ui_fits(badge, 1)) {
                    continue;
                }
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
                if !ui_fits(&buttons, buttons.len()) {
                    continue;
                }
                set_if_changed(&mut loaded.toolbar, buttons, &mut dirty.toolbar);
            }
            HostCommand::OpenModal { modal } => {
                if !ui_fits(&modal, modal.widgets.iter().map(widget_elements).sum()) {
                    continue;
                }
                if loaded.modal.is_none() {
                    loaded.modal_order = NEXT_MODAL_ORDER.fetch_add(1, Ordering::Relaxed);
                }
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
            HostCommand::RefreshSource { ref source_id }
                if !loaded
                    .effective_sources()
                    .iter()
                    .any(|s| s.id == *source_id) => {}
            other => forward.push(other),
        }
    }
    if state_changed {
        if let Some(writer) = &loaded.state_writer {
            writer.enqueue(&loaded.manifest.id, Arc::clone(&loaded.state));
        } else if matches!(loaded.source, PluginSource::Builtin) {
            save_plugin_state(state_dir, &loaded.manifest.id, &loaded.state);
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
