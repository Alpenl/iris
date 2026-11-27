//! Headless application orchestration layer (`iris-app`).
//!
//! Coordinates `iris-core` domain engines and the `iris-plugin-host` facade without
//! depending on `gpui`, native windowing crates, or `iris-mpv` directly.

pub mod playback;
pub mod plugins;

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
