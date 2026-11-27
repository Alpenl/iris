//! Headless application orchestration layer (`iris-app`).
//!
//! Coordinates `iris-core` domain engines and the `iris-plugin-host` facade without
//! depending on `gpui`, native windowing crates, or `iris-mpv` directly.

pub mod plugins;

pub use plugins::{
    ActiveSource, ActiveStreamMeta, OpenUrlOutcome, PendingHostAnnotation, PlayerProgress,
    PluginBatchOutcome, PluginContext, PluginController, PluginDomainChanges, PluginModalState,
    PluginPlayer, PluginSideEffect, PluginSourceView, StartedThumbnailFetch, ThumbKey,
    ThumbnailCache, ViewTimerSlot, can_seek_skip_range, error_code_label,
    find_plugin_playlist_index, remote_search_query, resolve_playlist_current, run_view_timer,
    should_record_progress, should_suppress_playback_error_toast,
};
