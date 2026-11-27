//! Single authoritative playback domain state (`PlaybackOrchestrator` / `PlaybackController`),
//! generation-bound `PendingSubtitles`, GPU reset recovery predicate, and sleep/NowPlaying targets.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::channel::mpsc::UnboundedReceiver;
use iris_core::library::{self, Kind};
use iris_core::player::{MediaStats, PlaybackState, Session, SessionUpdate};

use super::ports::{NowPlayingSnapshot, PlaybackSessionPort};
use super::shutdown::VideoAttachSlot;

pub const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
pub const NOW_PLAYING_MIN_INTERVAL: Duration = Duration::from_secs(2);
pub const VOLUME_ANNOUNCE_WINDOW: Duration = Duration::from_secs(1);

/// Concrete window playback controller backed by [`iris_core::player::Session`].
pub type PlaybackController = PlaybackOrchestrator<Session>;

/// Subtitle selections follow the load generation that owns them, including across folder scans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingSubtitles {
    generation: u64,
    paths: Vec<PathBuf>,
}

impl PendingSubtitles {
    pub fn add(&mut self, generation: u64, paths: Vec<PathBuf>) {
        if self.generation == generation && !self.paths.is_empty() {
            self.paths.extend(paths);
        } else {
            self.generation = generation;
            self.paths = paths;
        }
    }

    pub fn take(&mut self, generation: u64) -> Vec<PathBuf> {
        let paths = std::mem::take(&mut self.paths);
        if self.generation == generation {
            paths
        } else {
            Vec::new()
        }
    }

    pub fn advance(&mut self, from: u64, to: u64) {
        if self.generation == from && !self.paths.is_empty() {
            self.generation = to;
        }
    }

    pub fn clear(&mut self) {
        self.paths.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Splits user-opened paths into media files and external subtitle sidecars while preserving order.
pub fn split_open_paths(mut paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    paths.retain(|path| !path.as_os_str().is_empty());
    let subtitles = paths
        .extract_if(.., |path| library::is_subtitle(path))
        .collect();
    (paths, subtitles)
}

/// Pure predicate deciding whether `Session::recover_unloaded_after_gpu_reset` should run
/// after the 120ms settle window following a native `VideoSurface` device reset.
pub const fn should_recover_after_gpu_reset(
    reset_gen: u64,
    current_gen: u64,
    kind: Option<Kind>,
    loaded: bool,
    restored_vid: bool,
) -> bool {
    if reset_gen != current_gen {
        return false;
    }
    match kind {
        Some(Kind::Photo) => true,
        Some(Kind::Video) if restored_vid && loaded => false,
        _ => true,
    }
}

/// Pure target-state calculation for the platform display sleep preventer (`IOPMAssertion` / `SetThreadExecutionState`).
pub const fn should_prevent_display_sleep(kind: Option<Kind>, state: &PlaybackState) -> bool {
    matches!(kind, Some(Kind::Video)) && state.loaded && !state.paused && !state.ended
}

/// Computes the target Now Playing metadata snapshot for `session`, or `None` when idle/photo/unloaded.
pub fn compute_now_playing<S: PlaybackSessionPort>(session: &S) -> Option<NowPlayingSnapshot> {
    match session.kind() {
        Some(kind @ (Kind::Video | Kind::Audio)) if session.state().loaded => {
            let title = session.current_title().map(str::to_owned).or_else(|| {
                session
                    .current()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
            })?;
            Some(NowPlayingSnapshot {
                title,
                is_video: kind == Kind::Video,
            })
        }
        _ => None,
    }
}

/// Single authoritative owner of playback session state, `load_generation` fencing,
/// deferred external subtitles, stats cache, track/volume OSD announcement flags,
/// and Now Playing throttle state.
pub struct PlaybackOrchestrator<S = Session> {
    pub(super) session: S,
    pub(super) surface_attached: bool,
    pub(super) load_generation: u64,
    pub(super) pending_path: Option<PathBuf>,
    pub(super) pending_subtitles: PendingSubtitles,
    pub(super) loading_visible: bool,
    pub(super) pending_photo_actual: bool,
    pub(super) cached_stats: Option<MediaStats>,
    pub(super) last_stats_refresh: Instant,
    pub(super) announce_audio: bool,
    pub(super) announce_volume: Option<Instant>,
    pub(super) announce_sub: bool,
    pub(super) last_now_playing: Instant,
}

impl PlaybackOrchestrator<Session> {
    /// Initializes a window-bound `PlaybackController` and extracts its single `SessionUpdate` stream.
    pub fn open_window(
        session: Session,
        now: Instant,
    ) -> Result<(Self, UnboundedReceiver<SessionUpdate>)> {
        let rx = session
            .events()
            .ok_or_else(|| anyhow!("session event receiver already taken"))?;
        Ok((Self::new(session, now), rx))
    }

    /// Issues a narrow one-shot [`VideoAttachSlot`] token for `crates/iris/src/video::attach`
    /// during window initialization without `iris-app` ever calling `render_handle()` or `video_recovery()`.
    pub fn take_video_attach_slot(&mut self) -> Option<VideoAttachSlot<'_>> {
        if self.surface_attached {
            return None;
        }
        self.surface_attached = true;
        Some(VideoAttachSlot::new(&self.session))
    }
}

impl<S> PlaybackOrchestrator<S> {
    pub fn new(session: S, now: Instant) -> Self {
        Self {
            session,
            surface_attached: false,
            load_generation: 0,
            pending_path: None,
            pending_subtitles: PendingSubtitles::default(),
            loading_visible: false,
            pending_photo_actual: false,
            cached_stats: None,
            last_stats_refresh: now,
            announce_audio: false,
            announce_volume: None,
            announce_sub: false,
            last_now_playing: now.checked_sub(Duration::from_secs(10)).unwrap_or(now),
        }
    }

    pub fn session(&self) -> &S {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut S {
        &mut self.session
    }

    pub const fn load_generation(&self) -> u64 {
        self.load_generation
    }

    pub fn pending_path(&self) -> Option<&Path> {
        self.pending_path.as_deref()
    }

    pub const fn loading_visible(&self) -> bool {
        self.loading_visible
    }

    pub fn take_pending_photo_actual(&mut self) -> bool {
        std::mem::replace(&mut self.pending_photo_actual, false)
    }

    pub fn cached_stats(&self) -> Option<&MediaStats> {
        self.cached_stats.as_ref()
    }
}

impl<S: PlaybackSessionPort> PlaybackOrchestrator<S> {
    /// Target state for the platform display sleep preventer.
    pub fn should_prevent_sleep(&self) -> bool {
        should_prevent_display_sleep(self.session.kind(), self.session.state())
    }

    /// Concise media badge string (`LIVE · 1920×1080 · 硬解 videotoolbox`, colour profile, or `音频`).
    pub fn media_info(&self) -> Option<String> {
        let kind = self.session.kind()?;
        let st = self.session.state();
        let (w, h) = self.session.display_size();
        let mut parts = Vec::new();
        if st.is_live {
            parts.push("LIVE".into());
        }
        if w > 0 && h > 0 && kind != Kind::Audio {
            parts.push(format!("{w}×{h}"));
        }
        match kind {
            Kind::Video => parts.push(if st.hwdec.is_empty() || st.hwdec == "no" {
                "软解".into()
            } else {
                format!("硬解 {}", st.hwdec)
            }),
            Kind::Photo => {
                if let Some(colour) = st.photo_colour {
                    parts.push(colour.to_string());
                }
            }
            Kind::Audio => parts.push("音频".into()),
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Shuts down the owned playback session (MUST be called only after `VideoSurface` is dropped).
    pub fn shutdown(&self) {
        self.session.shutdown();
    }
}
