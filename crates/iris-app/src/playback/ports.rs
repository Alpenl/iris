//! Headless playback session port and UI-decoupled domain change DTOs.
//!
//! Intentionally free of window/chrome visibility flags (`fullscreen`, `chrome_visible`,
//! `sidebar_visible`, `settings_open`, `palette_open`, `popover_open`) and free of UI
//! region names (`DirtyRegions` mapping belongs exclusively to `crates/iris/src/ui/views.rs`).

use std::path::Path;

use anyhow::Result;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    AspectMode, AudioDevice, Command, MediaStats, PlaybackState, Session, SessionEvent,
    SessionUpdate, Track, TrackKind, coalesce_updates,
};
use iris_core::settings::{Hwdec, OnEof, Settings, SubLang};

/// Narrow headless port over [`Session`] enabling fast zero-libmpv unit tests of
/// [`super::PlaybackOrchestrator`] alongside full [`Session`] integration tests.
pub trait PlaybackSessionPort {
    type Update;

    fn coalesce_updates(batch: Vec<Self::Update>) -> Vec<Self::Update>;
    fn state(&self) -> &PlaybackState;
    fn kind(&self) -> Option<Kind>;
    fn current(&self) -> Option<&Path>;
    fn current_uri(&self) -> Option<&str>;
    fn current_media_uri(&self) -> Option<MediaUri>;
    fn current_title(&self) -> Option<&str>;
    fn may_share_current_identity(&self) -> bool;
    fn display_size(&self) -> (i64, i64);
    fn display_rotation(&self) -> u16;
    fn display_flips(&self) -> (bool, bool);
    fn tracks_of(&self, kind: TrackKind) -> Vec<&Track>;
    fn selected_track(&self, kind: TrackKind) -> Option<&Track>;
    fn audio_devices(&self) -> Vec<AudioDevice> {
        vec![AudioDevice {
            name: "auto".into(),
            description: String::new(),
        }]
    }
    fn media_stats(&self) -> MediaStats;
    fn refresh_dynamic_stats(&self, stats: &mut MediaStats);
    fn apply_update<'a>(&mut self, update: &'a Self::Update) -> Option<&'a SessionEvent>;
    fn flush_deferred(&mut self);
    fn execute(&mut self, command: Command) -> Result<()>;
    fn set_hwdec(&mut self, _hwdec: Hwdec) {}
    fn set_sub_lang(&mut self, _sub_lang: SubLang) {}
    fn set_sub_scale(&mut self, _scale: f64) {}
    fn set_on_eof_loop(&mut self, _on_eof: OnEof) {}
    fn apply_settings(&mut self, _settings: &Settings) {}
    fn transform(&mut self, _zoom: f64, _pan: [f64; 2]) -> Result<()> {
        Ok(())
    }
    fn subtitle_inset(&mut self, _logical_inset: f64, _viewport_height: f64) {}
    fn recover_unloaded_after_gpu_reset(&mut self, settings: &Settings, zoom: f64, pan: [f64; 2]);
    fn shutdown(&self);
}

impl PlaybackSessionPort for Session {
    type Update = SessionUpdate;

    fn coalesce_updates(batch: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
        coalesce_updates(batch)
    }

    fn state(&self) -> &PlaybackState {
        Session::state(self)
    }

    fn kind(&self) -> Option<Kind> {
        Session::kind(self)
    }

    fn current(&self) -> Option<&Path> {
        Session::current(self)
    }

    fn current_uri(&self) -> Option<&str> {
        Session::current_uri(self)
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        Session::current_media_uri(self)
    }

    fn current_title(&self) -> Option<&str> {
        Session::current_title(self)
    }

    fn may_share_current_identity(&self) -> bool {
        Session::may_share_current_identity(self)
    }

    fn display_size(&self) -> (i64, i64) {
        Session::display_size(self)
    }

    fn display_rotation(&self) -> u16 {
        Session::display_rotation(self)
    }

    fn display_flips(&self) -> (bool, bool) {
        Session::display_flips(self)
    }

    fn tracks_of(&self, kind: TrackKind) -> Vec<&Track> {
        Session::tracks_of(self, kind)
    }

    fn selected_track(&self, kind: TrackKind) -> Option<&Track> {
        Session::selected_track(self, kind)
    }

    fn audio_devices(&self) -> Vec<AudioDevice> {
        Session::audio_devices(self)
    }

    fn media_stats(&self) -> MediaStats {
        Session::media_stats(self)
    }

    fn refresh_dynamic_stats(&self, stats: &mut MediaStats) {
        Session::refresh_dynamic_stats(self, stats);
    }

    fn apply_update<'a>(&mut self, update: &'a SessionUpdate) -> Option<&'a SessionEvent> {
        Session::apply_update(self, update)
    }

    fn flush_deferred(&mut self) {
        Session::flush_deferred(self);
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        Session::execute(self, command)
    }

    fn set_hwdec(&mut self, hwdec: Hwdec) {
        Session::set_hwdec(self, hwdec);
    }

    fn set_sub_lang(&mut self, sub_lang: SubLang) {
        Session::set_sub_lang(self, sub_lang);
    }

    fn set_sub_scale(&mut self, scale: f64) {
        Session::set_sub_scale(self, scale);
    }

    fn set_on_eof_loop(&mut self, on_eof: OnEof) {
        Session::set_on_eof_loop(self, on_eof);
    }

    fn apply_settings(&mut self, settings: &Settings) {
        Session::apply_settings(self, settings);
    }

    fn transform(&mut self, zoom: f64, pan: [f64; 2]) -> Result<()> {
        Session::transform(self, zoom, pan)
    }

    fn subtitle_inset(&mut self, logical_inset: f64, viewport_height: f64) {
        Session::subtitle_inset(self, logical_inset, viewport_height);
    }

    fn recover_unloaded_after_gpu_reset(&mut self, settings: &Settings, zoom: f64, pan: [f64; 2]) {
        Session::recover_unloaded_after_gpu_reset(self, settings, zoom, pan);
    }

    fn shutdown(&self) {
        Session::shutdown(self);
    }
}

/// Pure semantic playback domain changes produced by [`super::PlaybackOrchestrator`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackDomainChanges {
    pub media_kind: Option<Kind>,
    pub loaded_or_reconfigured: bool,
    pub ended_settled: bool,
    pub buffering_changed: bool,
    pub duration_changed: bool,
    pub pause_or_speed_changed: bool,
    pub seeked: bool,
    pub audio_track_changed: bool,
    pub subtitle_track_changed: bool,
    pub volume_changed: bool,
    pub error_surfaced: bool,
    pub other_session_changed: bool,
}

impl PlaybackDomainChanges {
    /// Returns true when track/speed/duration/load state changed in a way that affects an open transport popover.
    pub const fn affects_transport_popover(&self) -> bool {
        self.audio_track_changed
            || self.subtitle_track_changed
            || self.pause_or_speed_changed
            || self.duration_changed
            || self.loaded_or_reconfigured
    }
}

/// EOF transition decision emitted when playback newly reaches `Ended(true)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EofOutcome {
    #[default]
    None,
    SaveAndStepNext,
    SaveAndSettle,
}

/// Target metadata for the OS Now Playing / System Media Transport Controls bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlayingSnapshot {
    pub title: String,
    pub is_video: bool,
}

/// Structured report produced from a coalesced batch of [`SessionUpdate`]s.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackEventReport {
    pub changes: PlaybackDomainChanges,
    pub position: Option<f64>,
    pub cache_duration_changed: bool,
    pub duration: f64,
    pub paused: bool,
    pub ended: bool,
    pub cache_end: Option<f64>,
    pub clock_changed: bool,
    pub should_flush_subtitles: bool,
    pub sleep_prevent_active: bool,
    /// `Some(Some(info))` updates OS Now Playing, `Some(None)` clears it, `None` keeps prior state (throttled).
    pub now_playing_updated: Option<Option<NowPlayingSnapshot>>,
    pub eof_outcome: EofOutcome,
    pub osd_messages: Vec<String>,
    pub error_code: Option<i32>,
}

/// Result of a user-initiated playback or visual-transform command.
#[derive(Debug, Default)]
pub struct PlaybackActionOutcome {
    pub osd: Option<String>,
    pub error: Option<anyhow::Error>,
    pub reset_image_view: bool,
    pub flip_pan_axis: Option<usize>,
}

/// Normalized hardware/system media key input routed into [`super::PlaybackOrchestrator`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaKeyInput {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    Seek(f64),
}

/// Action taken by [`super::PlaybackOrchestrator::handle_media_key`].
#[derive(Debug)]
pub enum MediaKeyOutcome {
    Ignored,
    Executed,
    StepQueue { delta: isize },
    Failed(anyhow::Error),
}

/// Outcome of flushing deferred external subtitle files for the active media generation.
#[derive(Debug)]
pub enum SubtitleFlushOutcome {
    Empty,
    MediaNotReady,
    Loaded {
        count: usize,
    },
    PartialFailure {
        count: usize,
        failed: usize,
        last_error: anyhow::Error,
    },
}

/// Localized display label for an [`AspectMode`].
pub fn aspect_label(mode: AspectMode) -> &'static str {
    match mode {
        AspectMode::Original => "原始比例",
        AspectMode::Ratio16x9 => "16:9",
        AspectMode::Ratio4x3 => "4:3",
        AspectMode::Ratio21x9 => "21:9",
        AspectMode::FillCrop => "铺满裁剪",
    }
}

/// Localized display label for an audio or subtitle [`Track`].
pub fn track_label(track: &Track) -> String {
    let mut parts = Vec::new();
    if !track.title.is_empty() {
        parts.push(track.title.clone());
    }
    if !track.lang.is_empty()
        && !track
            .title
            .to_ascii_lowercase()
            .contains(&track.lang.to_ascii_lowercase())
    {
        parts.push(track.lang.clone());
    }
    if !track.codec.is_empty() {
        parts.push(track.codec.to_ascii_uppercase());
    }
    if parts.is_empty() {
        parts.push(format!("轨道 {}", track.id));
    }
    let mut text = parts.join(" · ");
    if track.external {
        text.push_str(" [外挂]");
    }
    text
}
