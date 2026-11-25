//! Cohesive UI domain state structs: overlays, dialogs, active catalog state,
//! scrub/pointer tracking, escape-dismissal priority, and bounded LRU thumbnail cache.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{RenderImage, ScrollHandle, SharedString, UniformListScrollHandle};
use iris_api::{PluginModal, SourceEntry, StreamVariant};
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

pub(super) struct PendingBrowse {
    pub(super) req_id: String,
    pub(super) deadline: Instant,
    pub(super) plugin_id: String,
    pub(super) source_id: String,
    pub(super) append: bool,
    pub(super) _timeout: Option<gpui::Task<()>>,
}

pub(super) struct PendingResolve {
    pub(super) req_id: String,
    pub(super) deadline: Instant,
    pub(super) plugin_id: String,
    pub(super) source_id: String,
    pub(super) entry_id: String,
    pub(super) title: Option<String>,
    pub(super) _timeout: Option<gpui::Task<()>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SourceErrorKind {
    Browse { source_id: String },
    Resolve,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StreamReply {
    Requested {
        source_id: String,
        entry_id: String,
        title: Option<String>,
    },
}

/// Dropping the window or superseding a load must wake its blocking network read.
pub(super) struct RemoteLoad {
    pub(super) cancel: iris_core::vfs::HttpCancel,
    pub(super) plugin_id: Option<String>,
    pub(super) source_id: Option<String>,
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
    pub(super) active_stream_meta: Option<ActiveStreamMeta>,
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
    pub(super) pending_browse: Option<PendingBrowse>,
    pub(super) pending_resolve: Option<PendingResolve>,
    pub(super) remote_load: Option<RemoteLoad>,
    pub(super) directory_refresh: Option<gpui::Task<()>>,
    pub(super) filmstrip_index: usize,
    pub(super) remote_work_slot: Arc<futures::lock::Mutex<()>>,
}

impl CatalogState {
    pub(super) fn cancel_browse(&mut self) -> Option<PendingBrowse> {
        let pending = self.pending_browse.take()?;
        if let ActiveSource::Plugin(view) = &mut self.active_source
            && view.plugin_id == pending.plugin_id
            && view.source_id == pending.source_id
        {
            view.loading = false;
        }
        Some(pending)
    }

    pub(super) fn cancel_resolve(&mut self) -> Option<PendingResolve> {
        self.pending_resolve.take()
    }

    pub(super) fn take_error(&mut self, plugin_id: &str, req_id: &str) -> Option<SourceErrorKind> {
        if self
            .pending_resolve
            .as_ref()
            .is_some_and(|r| r.plugin_id == plugin_id && r.req_id == req_id)
        {
            self.pending_resolve = None;
            return Some(SourceErrorKind::Resolve);
        }
        if self
            .pending_browse
            .as_ref()
            .is_some_and(|b| b.plugin_id == plugin_id && b.req_id == req_id)
        {
            let pending = self.cancel_browse()?;
            return Some(SourceErrorKind::Browse {
                source_id: pending.source_id,
            });
        }
        None
    }

    pub(super) fn accept_stream(
        &mut self,
        plugin_id: &str,
        req_id: Option<&str>,
    ) -> Option<StreamReply> {
        let id = req_id?;
        let matches = self.pending_resolve.as_ref().is_some_and(|r| {
            r.plugin_id == plugin_id && r.req_id == id && Instant::now() < r.deadline
        });
        if !matches {
            return None;
        }
        let pending = self.pending_resolve.take()?;
        Some(StreamReply::Requested {
            source_id: pending.source_id,
            entry_id: pending.entry_id,
            title: pending.title,
        })
    }

    pub(super) fn take_page(
        &mut self,
        plugin_id: &str,
        req_id: &str,
        source_id: &str,
    ) -> Option<bool> {
        let matches = self.pending_browse.as_ref().is_some_and(|b| {
            b.plugin_id == plugin_id
                && b.req_id == req_id
                && b.source_id == source_id
                && Instant::now() < b.deadline
        });
        if !matches {
            return None;
        }
        let pending = self.cancel_browse()?;
        Some(pending.append)
    }

    pub(super) fn has_browse_for(&self, pid: Option<&str>, sid: Option<&str>) -> bool {
        self.pending_browse.as_ref().is_some_and(|b| {
            pid.is_none_or(|p| p == b.plugin_id) && sid.is_none_or(|s| s == b.source_id)
        })
    }

    pub(super) fn has_media_load_for(&self, pid: Option<&str>, sid: Option<&str>) -> bool {
        self.pending_resolve.as_ref().is_some_and(|r| {
            pid.is_none_or(|p| p == r.plugin_id) && sid.is_none_or(|s| s == r.source_id)
        }) || self.remote_load.as_ref().is_some_and(|load| {
            load.plugin_id.is_some()
                && pid.is_none_or(|p| load.plugin_id.as_deref() == Some(p))
                && sid.is_none_or(|s| load.source_id.as_deref() == Some(s))
        })
    }
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            sidebar_mode: SidebarMode::Files,
            active_source: ActiveSource::Local,
            local_dir: LocalDirectory::default(),
            active_stream_meta: None,
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
            pending_browse: None,
            pending_resolve: None,
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
    pub(super) submitting_plugin_modal: Option<(String, String)>,
    pub(super) plugin_modal_error: Option<SharedString>,
    pub(super) plugin_modal_focus_pending: bool,
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
            submitting_plugin_modal: None,
            plugin_modal_error: None,
            plugin_modal_focus_pending: false,
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
        self.palette_open = false;
        self.context_menu = Some(menu);
    }

    pub(super) fn open_dialog(&mut self, dialog: BuiltinDialog) {
        self.close_all();
        self.dialog_error = None;
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
        self.plugin_modal_error = None;
        self.submitting_plugin_modal = Some((pid.to_string(), modal.id.clone()));
        Some(buffers.collect_modal_values(pid, modal))
    }

    pub(super) fn clear_submitting_for_plugin(&mut self, pid: &str) -> bool {
        if self
            .submitting_plugin_modal
            .as_ref()
            .is_some_and(|(p, _)| p == pid)
        {
            self.submitting_plugin_modal = None;
            return true;
        }
        false
    }

    pub(super) fn cancel_plugin_modal_state(&mut self, buffers: &mut TextInputBuffers) {
        self.submitting_plugin_modal = None;
        self.plugin_modal_error = None;
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
            self.shortcut_conflict = None;
            EscapeStep::CancelRecording
        } else if has_plugin_modal {
            self.cancel_plugin_modal_state(buffers);
            EscapeStep::CancelPluginModal
        } else if self.dialog.take().is_some() {
            self.dialog_error = None;
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
    use super::{
        ActiveSource, CatalogState, PendingBrowse, PendingResolve, PluginSourceView, RemoteLoad,
        SourceErrorKind, StreamReply,
    };
    use std::time::{Duration, Instant};

    fn browse(id: &str, append: bool) -> PendingBrowse {
        PendingBrowse {
            req_id: id.into(),
            deadline: Instant::now() + Duration::from_secs(30),
            plugin_id: "plugin".into(),
            source_id: "source".into(),
            append,
            _timeout: Some(gpui::Task::ready(())),
        }
    }

    fn resolve(id: &str) -> PendingResolve {
        PendingResolve {
            req_id: id.into(),
            deadline: Instant::now() + Duration::from_secs(30),
            plugin_id: "plugin".into(),
            source_id: "source".into(),
            entry_id: id.into(),
            title: Some("selected".into()),
            _timeout: Some(gpui::Task::ready(())),
        }
    }

    #[test]
    fn resolve_only_accepts_the_latest_explicit_reply_once() {
        let mut catalog = CatalogState {
            pending_resolve: Some(resolve("B")),
            ..Default::default()
        };
        assert_eq!(catalog.accept_stream("plugin", Some("A")), None);
        assert_eq!(catalog.accept_stream("plugin", Some("")), None);
        assert_eq!(catalog.accept_stream("other", Some("B")), None);
        assert_eq!(catalog.accept_stream("plugin", None), None);
        assert_eq!(
            catalog.accept_stream("plugin", Some("B")),
            Some(StreamReply::Requested {
                source_id: "source".into(),
                entry_id: "B".into(),
                title: Some("selected".into())
            })
        );
        assert_eq!(catalog.accept_stream("plugin", Some("B")), None);
        assert_eq!(catalog.accept_stream("plugin", Some("A")), None);
    }

    #[test]
    fn old_errors_cannot_dismiss_a_new_browse_or_consume_its_page() {
        let mut catalog = CatalogState {
            pending_browse: Some(browse("page-2", true)),
            ..Default::default()
        };
        assert_eq!(catalog.take_error("plugin", "page-1"), None);
        assert_eq!(catalog.accept_stream("plugin", Some("page-2")), None);
        assert_eq!(catalog.take_page("plugin", "page-2", "other-source"), None);
        assert!(catalog.pending_browse.is_some());
        assert_eq!(catalog.take_page("plugin", "page-2", "source"), Some(true));
        assert!(catalog.pending_browse.is_none());
        assert_eq!(catalog.take_page("plugin", "page-2", "source"), None);
    }

    #[test]
    fn browse_and_resolve_lifecycles_never_clobber_each_other() {
        let cancel = iris_core::vfs::HttpCancel::default();
        let mut catalog = CatalogState {
            pending_browse: Some(browse("browse-1", false)),
            pending_resolve: Some(resolve("resolve-1")),
            remote_load: Some(RemoteLoad {
                cancel: cancel.clone(),
                plugin_id: Some("plugin".into()),
                source_id: Some("source".into()),
                _task: gpui::Task::ready(()),
            }),
            ..Default::default()
        };
        // Navigating or cancelling sidebar browse leaves in-flight resolve & remote_load intact.
        assert!(catalog.cancel_browse().is_some());
        assert!(!cancel.is_cancelled());
        assert_eq!(
            catalog.accept_stream("plugin", Some("resolve-1")),
            Some(StreamReply::Requested {
                source_id: "source".into(),
                entry_id: "resolve-1".into(),
                title: Some("selected".into())
            })
        );
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn timeout_and_navigation_discard_late_replies() {
        let mut catalog = CatalogState {
            pending_resolve: Some(resolve("old")),
            ..Default::default()
        };
        catalog.cancel_resolve();
        assert_eq!(catalog.accept_stream("plugin", Some("old")), None);

        let mut expired = resolve("slow");
        expired.deadline = Instant::now();
        catalog.pending_resolve = Some(expired);
        assert_eq!(catalog.accept_stream("plugin", Some("slow")), None);
        assert_eq!(
            catalog.take_error("plugin", "slow"),
            Some(SourceErrorKind::Resolve)
        );
        assert_eq!(catalog.accept_stream("plugin", Some("slow")), None);
    }

    #[test]
    fn silent_browse_expires_and_releases_its_loading_indicator() {
        let mut slow = browse("slow", false);
        slow.deadline = Instant::now();
        let mut catalog = CatalogState {
            pending_browse: Some(slow),
            active_source: ActiveSource::Plugin(Box::new(PluginSourceView {
                plugin_id: "plugin".into(),
                plugin_name: "Plugin".into(),
                source_id: "source".into(),
                source_name: "Source".into(),
                supports_search: false,
                auth_command_id: None,
                folder_id: None,
                parent_id: None,
                title: String::new(),
                entries: Vec::new(),
                next_page: None,
                authenticated: true,
                auth_hint: None,
                loading: true,
                error: None,
            })),
            ..Default::default()
        };
        assert_eq!(catalog.take_page("plugin", "slow", "source"), None);
        assert_eq!(catalog.take_error("plugin", "earlier"), None);
        assert!(matches!(&catalog.active_source, ActiveSource::Plugin(v) if v.loading));
        assert_eq!(
            catalog.take_error("plugin", "slow"),
            Some(SourceErrorKind::Browse {
                source_id: "source".into()
            })
        );
        assert!(matches!(&catalog.active_source, ActiveSource::Plugin(v) if !v.loading));
        assert!(catalog.pending_browse.is_none());
        assert_eq!(catalog.take_page("plugin", "slow", "source"), None);
    }

    #[test]
    fn replacing_remote_load_cancels_reader_and_serializes_preparation() {
        let cancel = iris_core::vfs::HttpCancel::default();
        let mut catalog = CatalogState {
            remote_load: Some(RemoteLoad {
                cancel: cancel.clone(),
                plugin_id: Some("plugin".into()),
                source_id: Some("source".into()),
                _task: gpui::Task::ready(()),
            }),
            ..Default::default()
        };
        let active = catalog.remote_work_slot.try_lock().unwrap();
        assert!(catalog.has_media_load_for(Some("plugin"), Some("source")));
        catalog.remote_load = None;
        assert!(cancel.is_cancelled());
        assert!(catalog.remote_work_slot.try_lock().is_none());
        drop(active);
        assert!(catalog.remote_work_slot.try_lock().is_some());
    }
}
