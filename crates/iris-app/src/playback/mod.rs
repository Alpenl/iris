//! Headless playback orchestration vertical slice (`crates/iris-app/src/playback/`).
//!
//! Owns `Session`, `load_generation` fencing, `PendingSubtitles`, `MediaStats` cache,
//! track/volume OSD announcement flags, GPU reset recovery evaluation, sleep-preventer
//! and NowPlaying target computation, narrow one-shot `VideoAttachSlot` issuance, and
//! the explicit two-phase `WindowShutdownCoordinator`.

mod actions;
mod events;
mod ports;
mod shutdown;
mod state;

pub use ports::{
    EofOutcome, MediaKeyInput, MediaKeyOutcome, NowPlayingSnapshot, PlaybackActionOutcome,
    PlaybackDomainChanges, PlaybackEventReport, PlaybackSessionPort, SubtitleFlushOutcome,
    aspect_label, track_label,
};
pub use shutdown::{VideoAttachSlot, WindowShutdownCoordinator};
pub use state::{
    NOW_PLAYING_MIN_INTERVAL, PendingSubtitles, PlaybackController, PlaybackOrchestrator,
    STATS_REFRESH_INTERVAL, VOLUME_ANNOUNCE_WINDOW, compute_now_playing,
    should_prevent_display_sleep, should_recover_after_gpu_reset, split_open_paths,
};

#[cfg(test)]
mod tests;
