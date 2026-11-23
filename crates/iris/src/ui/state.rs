//! Cohesive UI domain state structs: overlays, dialogs, active catalog state,
//! scrub/pointer tracking, escape-dismissal priority, and bounded LRU thumbnail cache.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{RenderImage, ScrollHandle, UniformListScrollHandle};
use iris_api::{PluginModal, SourceEntry, StreamVariant};
use iris_core::catalog::{CatalogItem, MediaFilter};
use iris_core::preset::{PresetCaptureScope, SidebarMode};

use super::context_menu::ContextMenu;
use super::kit::text_buffer::TextInputBuffers;
use super::sidebar::DirRow;

/// Cap in-memory decoded thumbnails to keep RAM usage tiny.
pub(super) const MAX_THUMB_CACHE: usize = 80;
/// Maximum simultaneous background thumbnail decode tasks.
pub(super) const MAX_THUMB_CONCURRENT: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PopoverKind {
    Subtitles,
    Audio,
    Speed,
    More,
    Presets,
    Variants,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SettingsTab {
    Appearance,
    Presets,
    Playback,
    Tracks,
    View,
    Plugins,
    Shortcuts,
}

/// Host built-in lightweight modals (`OpenUrl`, `SavePreset`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinDialog {
    OpenUrl,
    SavePreset { scope: PresetCaptureScope },
}

/// Deterministic single-step outcome of pressing `Esc` in `Workspace && !TextInput`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EscapeStep {
    CancelRecording,
    CancelPluginModal,
    CloseBuiltinDialog,
    CloseContextMenu,
    ClosePalette,
    CloseSettings,
    ClosePopover,
    CloseInfo,
    CloseDirPopup,
    CloseSearch,
    ExitFullscreen,
    Noop,
}

/// Active plugin `MediaSource` browsing state inside [`ActiveSource::Plugin`].
#[derive(Clone, Debug)]
pub(super) struct PluginSourceView {
    pub(super) plugin_id: String,
    pub(super) plugin_name: String,
    pub(super) source_id: String,
    pub(super) source_name: String,
    pub(super) supports_search: bool,
    pub(super) auth_command_id: Option<String>,
    pub(super) folder_id: Option<String>,
    pub(super) parent_id: Option<String>,
    pub(super) title: String,
    pub(super) entries: Vec<SourceEntry>,
    pub(super) next_page: Option<String>,
    pub(super) authenticated: bool,
    pub(super) auth_hint: Option<String>,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    pub(super) active_request_id: String,
}

/// Tracks which media source is currently browsed in the sidebar `Files` view.
#[derive(Clone, Debug, Default)]
pub(super) enum ActiveSource {
    #[default]
    Local,
    Plugin(Box<PluginSourceView>),
}

impl ActiveSource {
    pub(super) fn is_local(&self) -> bool {
        matches!(self, ActiveSource::Local)
    }

    pub(super) fn display_badge(&self) -> Option<String> {
        match self {
            ActiveSource::Local => None,
            ActiveSource::Plugin(ps) => Some(format!("{} · {}", ps.plugin_name, ps.source_name)),
        }
    }
}

/// Metadata for a currently playing plugin stream with switchable quality variants.
#[derive(Clone, Debug, Default)]
pub(super) struct ActiveStreamMeta {
    pub(super) plugin_id: Option<String>,
    pub(super) source_id: Option<String>,
    pub(super) entry_id: Option<String>,
    pub(super) variants: Vec<StreamVariant>,
    pub(super) active_variant: Option<String>,
}

/// Pending asynchronous `SourceResolve` request metadata.
#[derive(Clone, Debug)]
pub(super) struct PendingResolve {
    pub(super) req_id: String,
    pub(super) plugin_id: String,
    pub(super) source_id: String,
    pub(super) entry_id: String,
    pub(super) title: Option<String>,
}

/// Unified sidebar catalog & virtual-list browsing state.
pub(super) struct CatalogState {
    pub(super) sidebar_mode: SidebarMode,
    pub(super) active_source: ActiveSource,
    pub(super) active_stream_meta: Option<ActiveStreamMeta>,
    pub(super) filter: MediaFilter,
    pub(super) rows: Vec<DirRow>,
    pub(super) items: Vec<CatalogItem>,
    pub(super) photo_paths: Vec<PathBuf>,
    pub(super) video_audio_count: usize,
    pub(super) photo_count: usize,
    pub(super) favorite_count: usize,
    pub(super) thumbs: ThumbCache,
    pub(super) dir_scroll: UniformListScrollHandle,
    pub(super) source_req_seq: u64,
    pub(super) pending_resolve: Option<PendingResolve>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            sidebar_mode: SidebarMode::Files,
            active_source: ActiveSource::Local,
            active_stream_meta: None,
            filter: MediaFilter::All,
            rows: Vec::new(),
            items: Vec::new(),
            photo_paths: Vec::new(),
            video_audio_count: 0,
            photo_count: 0,
            favorite_count: 0,
            thumbs: ThumbCache::default(),
            dir_scroll: UniformListScrollHandle::new(),
            source_req_seq: 0,
            pending_resolve: None,
        }
    }
}

/// Manages mutually exclusive modals, popovers, command palette, and shortcut recorder.
pub(super) struct OverlayState {
    pub(super) dir_popup: bool,
    pub(super) settings_open: bool,
    pub(super) settings_tab: SettingsTab,
    pub(super) info_open: bool,
    pub(super) popover: Option<PopoverKind>,
    pub(super) context_menu: Option<ContextMenu>,
    pub(super) palette_open: bool,
    pub(super) palette_index: usize,
    pub(super) palette_scroll: ScrollHandle,
    pub(super) dialog: Option<BuiltinDialog>,
    pub(super) recording_shortcut: Option<(String, String)>,
    pub(super) shortcut_conflict: Option<String>,
    pub(super) submitting_plugin_modal: Option<(String, String)>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            dir_popup: false,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            info_open: false,
            popover: None,
            context_menu: None,
            palette_open: false,
            palette_index: 0,
            palette_scroll: ScrollHandle::new(),
            dialog: None,
            recording_shortcut: None,
            shortcut_conflict: None,
            submitting_plugin_modal: None,
        }
    }
}

impl OverlayState {
    pub(super) fn has_blocking(&self) -> bool {
        self.dir_popup
            || self.popover.is_some()
            || self.settings_open
            || self.palette_open
            || self.context_menu.is_some()
            || self.dialog.is_some()
    }

    /// True when single-key playback bindings must be suppressed (`TextInput` KeyContext).
    pub(super) fn is_text_input_active(
        &self,
        buffers: &TextInputBuffers,
        search_focused: bool,
        palette_focused: bool,
        form_focused: bool,
    ) -> bool {
        buffers.is_focused_given(search_focused, palette_focused, form_focused)
            || self.recording_shortcut.is_some()
    }

    pub(super) fn close_menus(&mut self) {
        self.popover = None;
        self.context_menu = None;
    }

    pub(super) fn close_all(&mut self) {
        self.dir_popup = false;
        self.popover = None;
        self.settings_open = false;
        self.palette_open = false;
        self.context_menu = None;
        self.dialog = None;
        self.recording_shortcut = None;
    }

    pub(super) fn open_settings(&mut self, tab: Option<SettingsTab>) {
        self.close_all();
        if let Some(t) = tab {
            self.settings_tab = t;
        }
        self.settings_open = true;
    }

    pub(super) fn toggle_settings(&mut self) -> bool {
        if self.settings_open {
            self.settings_open = false;
            self.recording_shortcut = None;
        } else {
            self.open_settings(None);
        }
        self.settings_open
    }

    pub(super) fn toggle_palette(&mut self) -> bool {
        if self.palette_open {
            self.palette_open = false;
        } else {
            self.close_all();
            self.palette_open = true;
        }
        self.palette_open
    }

    pub(super) fn toggle_popover(&mut self, kind: PopoverKind) {
        self.context_menu = None;
        self.palette_open = false;
        self.dialog = None;
        if self.popover == Some(kind) {
            self.popover = None;
        } else {
            self.popover = Some(kind);
            self.settings_open = false;
            self.recording_shortcut = None;
        }
    }

    pub(super) fn open_context_menu(&mut self, menu: ContextMenu) {
        self.popover = None;
        self.palette_open = false;
        self.context_menu = Some(menu);
    }

    pub(super) fn open_dialog(&mut self, dialog: BuiltinDialog) {
        self.close_all();
        self.dialog = Some(dialog);
    }

    /// Marks `(pid, modal.id)` as awaiting plugin validation without closing the modal
    /// or erasing `modal:*` fields; returns `None` to debounce duplicate rapid clicks.
    pub(super) fn begin_plugin_modal_submit(
        &mut self,
        buffers: &TextInputBuffers,
        pid: &str,
        modal: &PluginModal,
    ) -> Option<BTreeMap<String, String>> {
        if self
            .submitting_plugin_modal
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == &modal.id)
        {
            return None;
        }
        self.submitting_plugin_modal = Some((pid.to_string(), modal.id.clone()));
        Some(buffers.collect_modal_values(pid, modal))
    }

    pub(super) fn clear_submitting_for_plugin(&mut self, pid: &str) {
        if self
            .submitting_plugin_modal
            .as_ref()
            .is_some_and(|(p, _)| p == pid)
        {
            self.submitting_plugin_modal = None;
        }
    }

    pub(super) fn complete_plugin_modal_close(
        &mut self,
        buffers: &mut TextInputBuffers,
        next_modal: Option<(&str, &PluginModal)>,
    ) {
        self.submitting_plugin_modal = None;
        buffers.sync_plugin_modal(next_modal);
    }

    pub(super) fn cancel_plugin_modal_state(&mut self, buffers: &mut TextInputBuffers) {
        self.submitting_plugin_modal = None;
        buffers.clear_modal_fields();
    }

    /// Peels exactly one top-most UI layer on `Esc` without skipping levels.
    pub(super) fn dismiss_next_escape(
        &mut self,
        buffers: &mut TextInputBuffers,
        has_plugin_modal: bool,
        is_fullscreen: bool,
    ) -> EscapeStep {
        if self.recording_shortcut.take().is_some() {
            EscapeStep::CancelRecording
        } else if has_plugin_modal {
            self.cancel_plugin_modal_state(buffers);
            EscapeStep::CancelPluginModal
        } else if self.dialog.take().is_some() {
            buffers.active_form_key = None;
            EscapeStep::CloseBuiltinDialog
        } else if self.context_menu.take().is_some() {
            EscapeStep::CloseContextMenu
        } else if self.palette_open {
            self.palette_open = false;
            buffers.palette_input.clear();
            EscapeStep::ClosePalette
        } else if self.settings_open {
            self.settings_open = false;
            EscapeStep::CloseSettings
        } else if self.popover.take().is_some() {
            EscapeStep::ClosePopover
        } else if self.info_open {
            self.info_open = false;
            EscapeStep::CloseInfo
        } else if self.dir_popup {
            self.dir_popup = false;
            EscapeStep::CloseDirPopup
        } else if buffers.search_open {
            buffers.search_open = false;
            buffers.search_input.clear();
            EscapeStep::CloseSearch
        } else if is_fullscreen {
            EscapeStep::ExitFullscreen
        } else {
            EscapeStep::Noop
        }
    }
}

/// Timeline scrubbing, volume dragging, wheel damping, and double-click debounce.
pub(super) struct ScrubState {
    pub(super) timeline_hover: bool,
    pub(super) seeking: bool,
    pub(super) seek_preview_ratio: Option<f32>,
    pub(super) last_seek_drag: Instant,
    pub(super) dragging_volume: bool,
    pub(super) wheel_accum: f64,
    pub(super) click_seq: u64,
}

impl Default for ScrubState {
    fn default() -> Self {
        Self {
            timeline_hover: false,
            seeking: false,
            seek_preview_ratio: None,
            last_seek_drag: Instant::now(),
            dragging_volume: false,
            wheel_accum: 0.,
            click_seq: 0,
        }
    }
}

/// Bounded LRU thumbnail cache with priority decode queue and concurrency cap.
#[derive(Default)]
pub(super) struct ThumbCache {
    entries: HashMap<PathBuf, (u64, Arc<RenderImage>)>,
    pending: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    active_tasks: usize,
    tick: u64,
    generation: u64,
}

impl ThumbCache {
    pub(super) fn get(&self, path: &Path) -> Option<Arc<RenderImage>> {
        self.entries.get(path).map(|(_, img)| img.clone())
    }

    pub(super) fn touch(&mut self, path: &Path) {
        if let Some((stamp, _)) = self.entries.get_mut(path) {
            self.tick += 1;
            *stamp = self.tick;
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn enqueue_window(&mut self, prioritized: Vec<PathBuf>) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        for p in prioritized {
            if self.entries.contains_key(&p) {
                self.touch(&p);
            } else if !self.inflight.contains(&p) {
                self.pending.push_back(p);
            }
        }
    }

    pub(super) fn pop_next_batch(&mut self) -> Vec<(u64, PathBuf)> {
        let mut out = Vec::new();
        while self.active_tasks < MAX_THUMB_CONCURRENT {
            let Some(path) = self.pending.pop_front() else {
                break;
            };
            if self.entries.contains_key(&path) || self.inflight.contains(&path) {
                continue;
            }
            self.inflight.insert(path.clone());
            self.active_tasks += 1;
            out.push((self.generation, path));
        }
        out
    }

    pub(super) fn finish(&mut self, path: PathBuf, decoded: Option<Arc<RenderImage>>) {
        self.inflight.remove(&path);
        self.active_tasks = self.active_tasks.saturating_sub(1);
        if let Some(img) = decoded {
            self.tick += 1;
            let stamp = self.tick;
            if self.entries.len() >= MAX_THUMB_CACHE
                && !self.entries.contains_key(&path)
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (t, _))| *t)
                    .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
            self.entries.insert(path, (stamp, img));
        }
    }
}
