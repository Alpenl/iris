//! Headless application orchestration layer (`iris-app`).
//!
//! Coordinates `iris-core` domain engines and the `iris-plugin-host` facade without
//! depending on `gpui`, native windowing crates, or `iris-mpv` directly.

pub mod catalog;
pub mod model;
pub mod playback;
pub mod plugins;
pub mod workbench;

pub use catalog::{
    BookmarkAction, CatalogController, CatalogCounts, CompletedFavoriteScan, DirRow,
    DirectStreamTarget, FavoriteScanJob, LocalRefreshTicket, OpenBatchPlan, OpenMediaBatchAction,
    SAVE_EVERY, StepAction, StoreFlushIntent, StoreFlushReceipt, can_step_queue,
    fav_uri_to_catalog_item, parse_direct_stream_input, plan_open_paths, resolve_queue_paths,
};
pub use model::{
    AppModel, MediaOpenCommit, PresetDeleteOutcome, PresetSaveOutcome, PresetSwitchOutcome,
    ResetAppearanceOutcome,
};
#[cfg(any(debug_assertions, feature = "devtools"))]
pub use model::{DebugStateSnapshot, DebugUiSnapshot, parse_debug_filter};
pub use playback::{
    EofOutcome, MediaKeyInput, MediaKeyOutcome, NowPlayingSnapshot, PendingSubtitles,
    PlaybackActionOutcome, PlaybackController, PlaybackDomainChanges, PlaybackEventReport,
    PlaybackOrchestrator, PlaybackSessionPort, SubtitleFlushOutcome, VideoAttachSlot,
    WindowShutdownCoordinator, aspect_label, compute_now_playing, should_prevent_display_sleep,
    should_recover_after_gpu_reset, split_open_paths, track_label,
};
pub use plugins::{
    ActiveSource, ActiveStreamMeta, OpenUrlOutcome, PendingHostAnnotation, PlayerProgress,
    PluginBatchOutcome, PluginContext, PluginController, PluginDomainChanges, PluginModalState,
    PluginPlayer, PluginSideEffect, PluginSourceView, StartedThumbnailFetch, ThumbKey,
    ThumbnailCache, ViewTimerSlot, can_seek_skip_range, error_code_label,
    find_plugin_playlist_index, remote_search_query, resolve_playlist_current, run_view_timer,
    should_record_progress, should_suppress_playback_error_toast,
};
pub use workbench::{
    DOH_PRESET_CUSTOM, DOH_PRESET_DOMESTIC_FIRST, DOH_PRESET_DOMESTIC_ONLY,
    DOH_PRESET_INTERNATIONAL, PresetDeleteStep, PresetSaveStep, PresetSwitchStep,
    ShortcutRecordOutcome, StartupWorkbench, WorkbenchController, WorkbenchDomainChanges,
    apply_appearance_reset_defaults, doh_resolver_preset_for, doh_resolvers_for_preset,
    initial_window_size, sync_doh_settings,
};
