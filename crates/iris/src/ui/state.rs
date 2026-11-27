//! Cohesive UI domain state structs: overlays, dialogs, active catalog state,
//! scrub/pointer tracking, escape-dismissal priority, and bounded LRU thumbnail cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{RenderImage, ScrollHandle, SharedString, UniformListScrollHandle};
pub(super) use iris_app::ActiveSource;
use iris_core::catalog::{CatalogItem, LocalDirectory, MediaFilter};
use iris_core::preset::{PresetCaptureScope, SidebarMode};

use super::chrome::DirectoryPopup;
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

/// Host built-in lightweight modals (`OpenUrl`, `SavePreset`, `ConfirmUninstallPlugin`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinDialog {
    OpenUrl,
    SavePreset {
        scope: PresetCaptureScope,
    },
    ConfirmUninstallPlugin {
        plugin_id: String,
        plugin_name: String,
        keep_data: bool,
    },
}

/// Deterministic single-step outcome of pressing `Esc` across the entire window.
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
    BlurSidebarForm,
    ClearSearchText,
    CloseSearch,
    ExitFullscreen,
    Noop,
}

/// Dropping the window or superseding a load must wake its blocking network read.
pub(super) struct RemoteLoad {
    pub(super) cancel: iris_base::net::http_client::HttpCancel,
    pub(super) _task: gpui::Task<()>,
}

impl Drop for RemoteLoad {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Unified sidebar catalog & virtual-list browsing state.
pub(super) struct CatalogState {
    pub(super) sidebar_mode: SidebarMode,
    pub(super) active_source: ActiveSource,
    pub(super) local_dir: LocalDirectory,
    pub(super) filter: MediaFilter,
    pub(super) rows: Vec<DirRow>,
    pub(super) items: Vec<CatalogItem>,
    pub(super) playable_count: usize,
    pub(super) current_playable_number: Option<usize>,
    pub(super) sorted_source_items: Vec<CatalogItem>,
    pub(super) favorite_uris: Vec<iris_core::media_uri::MediaUri>,
    pub(super) favorite_items: Vec<CatalogItem>,
    pub(super) favorite_scan: Option<gpui::Task<()>>,
    pub(super) photo_paths: Vec<PathBuf>,
    pub(super) video_count: usize,
    pub(super) audio_count: usize,
    pub(super) photo_count: usize,
    pub(super) favorite_count: usize,
    pub(super) thumbs: ThumbCache,
    pub(super) dir_scroll: UniformListScrollHandle,
    pub(super) remote_load: Option<RemoteLoad>,
    pub(super) directory_refresh: Option<gpui::Task<()>>,
    pub(super) filmstrip_index: usize,
    pub(super) remote_work_slot: Arc<futures::lock::Mutex<()>>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            sidebar_mode: SidebarMode::Files,
            active_source: ActiveSource::Local,
            local_dir: LocalDirectory::default(),
            filter: MediaFilter::All,
            rows: Vec::new(),
            items: Vec::new(),
            playable_count: 0,
            current_playable_number: None,
            sorted_source_items: Vec::new(),
            favorite_uris: Vec::new(),
            favorite_items: Vec::new(),
            favorite_scan: None,
            photo_paths: Vec::new(),
            video_count: 0,
            audio_count: 0,
            photo_count: 0,
            favorite_count: 0,
            thumbs: ThumbCache::default(),
            dir_scroll: UniformListScrollHandle::new(),
            remote_load: None,
            directory_refresh: None,
            filmstrip_index: 0,
            remote_work_slot: Arc::new(futures::lock::Mutex::new(())),
        }
    }
}

/// Manages mutually exclusive modals, popovers, command palette, and shortcut recorder.
pub(super) struct OverlayState {
    pub(super) dir_popup: DirectoryPopup,
    pub(super) settings_open: bool,
    pub(super) settings_tab: SettingsTab,
    pub(super) info_open: bool,
    pub(super) popover: Option<PopoverKind>,
    pub(super) popover_index: Option<usize>,
    pub(super) context_menu: Option<ContextMenu>,
    pub(super) palette_open: bool,
    pub(super) palette_index: usize,
    pub(super) palette_scroll: ScrollHandle,
    pub(super) dialog: Option<BuiltinDialog>,
    pub(super) dialog_error: Option<SharedString>,
    pub(super) recording_shortcut: Option<(String, String)>,
    pub(super) shortcut_conflict: Option<String>,
    pub(super) pending_confirm: Option<(String, Instant)>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            dir_popup: DirectoryPopup::Hidden,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            info_open: false,
            popover: None,
            popover_index: None,
            context_menu: None,
            palette_open: false,
            palette_index: 0,
            palette_scroll: ScrollHandle::new(),
            dialog: None,
            dialog_error: None,
            recording_shortcut: None,
            shortcut_conflict: None,
            pending_confirm: None,
        }
    }
}

impl OverlayState {
    pub(super) fn has_menu_or_modal(&self) -> bool {
        self.popover.is_some()
            || self.settings_open
            || self.palette_open
            || self.context_menu.is_some()
            || self.dialog.is_some()
    }

    pub(super) fn is_recording(&self, focused: bool) -> bool {
        self.settings_open && self.recording_shortcut.is_some() && focused
    }

    pub(super) fn close_menus(&mut self) {
        self.popover = None;
        self.popover_index = None;
        self.context_menu = None;
    }

    pub(super) fn close_variants_popover(&mut self) {
        if self.popover == Some(PopoverKind::Variants) {
            self.popover = None;
            self.popover_index = None;
        }
    }

    pub(super) fn is_confirming(&self, key: &str, now: Instant) -> bool {
        self.pending_confirm
            .as_ref()
            .is_some_and(|(k, until)| k == key && now < *until)
    }

    pub(super) fn confirm_or_arm(
        &mut self,
        key: impl Into<String>,
        now: Instant,
        ttl: std::time::Duration,
    ) -> bool {
        let key = key.into();
        if self.is_confirming(&key, now) {
            self.pending_confirm = None;
            true
        } else {
            self.pending_confirm = Some((key, now + ttl));
            false
        }
    }

    pub(super) fn close_all(&mut self) {
        self.dir_popup = DirectoryPopup::Hidden;
        self.popover = None;
        self.popover_index = None;
        self.settings_open = false;
        self.palette_open = false;
        self.context_menu = None;
        self.dialog = None;
        self.dialog_error = None;
        self.recording_shortcut = None;
        self.pending_confirm = None;
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
        self.dialog_error = None;
        self.popover_index = None;
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
        self.popover_index = None;
        self.palette_open = false;
        self.settings_open = false;
        self.dialog = None;
        self.dialog_error = None;
        self.recording_shortcut = None;
        self.context_menu = Some(menu);
    }

    pub(super) fn open_dialog(&mut self, dialog: BuiltinDialog) {
        self.close_all();
        self.dialog_error = None;
        self.dialog = Some(dialog);
    }

    pub(super) fn open_dialog_over_settings(&mut self, dialog: BuiltinDialog) {
        self.popover = None;
        self.popover_index = None;
        self.context_menu = None;
        self.palette_open = false;
        self.recording_shortcut = None;
        self.dialog_error = None;
        self.dialog = Some(dialog);
    }

    /// Peels exactly one top-most UI layer on `Esc` without skipping levels.
    pub(super) fn dismiss_next_escape(
        &mut self,
        buffers: &mut TextInputBuffers,
        has_plugin_modal: bool,
        is_fullscreen: bool,
    ) -> EscapeStep {
        if self.recording_shortcut.take().is_some() {
            self.shortcut_conflict = None;
            EscapeStep::CancelRecording
        } else if has_plugin_modal {
            buffers.clear_modal_fields();
            EscapeStep::CancelPluginModal
        } else if self.dialog.take().is_some() {
            self.dialog_error = None;
            buffers.active_form_key = None;
            EscapeStep::CloseBuiltinDialog
        } else if self.palette_open {
            self.palette_open = false;
            buffers.palette_input.clear();
            EscapeStep::ClosePalette
        } else if self.settings_open {
            self.settings_open = false;
            EscapeStep::CloseSettings
        } else if self.context_menu.take().is_some() {
            EscapeStep::CloseContextMenu
        } else if self.popover.take().is_some() {
            EscapeStep::ClosePopover
        } else if self.info_open {
            self.info_open = false;
            EscapeStep::CloseInfo
        } else if self.dir_popup.is_open() {
            self.dir_popup = DirectoryPopup::Hidden;
            EscapeStep::CloseDirPopup
        } else if buffers
            .active_form_key
            .as_deref()
            .is_some_and(|k| k.starts_with("sb:"))
        {
            buffers.active_form_key = None;
            EscapeStep::BlurSidebarForm
        } else if buffers.search_open && !buffers.search_input.is_empty() {
            buffers.search_input.clear();
            EscapeStep::ClearSearchText
        } else if buffers.search_open {
            buffers.search_open = false;
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
    pub(super) seek_dragged: bool,
    pub(super) seek_preview_ratio: Option<f32>,
    pub(super) seek_preview_timeout: Option<gpui::Task<()>>,
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
            seek_dragged: false,
            seek_preview_ratio: None,
            seek_preview_timeout: None,
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
    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn get(&self, path: &Path) -> Option<Arc<RenderImage>> {
        self.entries.get(path).map(|(_, img)| img.clone())
    }

    pub(super) fn touch(&mut self, path: &Path) {
        if let Some((stamp, _)) = self.entries.get_mut(path) {
            self.tick += 1;
            *stamp = self.tick;
        }
    }

    pub(super) fn enqueue_window(&mut self, prioritized: Vec<PathBuf>) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        for p in prioritized {
            if self.entries.contains_key(&p) {
                self.touch(&p);
            } else {
                self.pending.push_back(p);
            }
        }
    }

    pub(super) fn pop_next_batch(&mut self) -> Vec<(u64, PathBuf)> {
        let mut out = Vec::new();
        while self.active_tasks < MAX_THUMB_CONCURRENT {
            let Some(index) = self
                .pending
                .iter()
                .position(|path| !self.inflight.contains(path))
            else {
                break;
            };
            let path = self.pending.remove(index).expect("pending thumbnail index");
            if self.entries.contains_key(&path) {
                continue;
            }
            self.inflight.insert(path.clone());
            self.active_tasks += 1;
            out.push((self.generation, path));
        }
        out
    }

    pub(super) fn finish(
        &mut self,
        generation: u64,
        path: PathBuf,
        decoded: Option<Arc<RenderImage>>,
    ) -> bool {
        self.inflight.remove(&path);
        self.active_tasks = self.active_tasks.saturating_sub(1);
        if generation == self.generation
            && let Some(img) = decoded
        {
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
            return true;
        }
        false
    }
}

#[cfg(test)]
mod remote_lifecycle_tests {
    use super::{CatalogState, RemoteLoad};

    #[test]
    fn replacing_remote_load_cancels_reader_and_serializes_preparation() {
        let cancel = iris_base::net::http_client::HttpCancel::default();
        let mut catalog = CatalogState {
            remote_load: Some(RemoteLoad {
                cancel: cancel.clone(),
                _task: gpui::Task::ready(()),
            }),
            ..Default::default()
        };
        let active = catalog.remote_work_slot.try_lock().unwrap();
        catalog.remote_load = None;
        assert!(cancel.is_cancelled());
        assert!(catalog.remote_work_slot.try_lock().is_none());
        drop(active);
        assert!(catalog.remote_work_slot.try_lock().is_some());
    }
}
