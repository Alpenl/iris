//! Coalesced `SessionUpdate` event reducer, `load_generation` lifecycle fencing,
//! subtitle flushing, stats refresh throttling, NowPlaying throttling, and GPU reset recovery.

use std::path::PathBuf;
use std::time::Instant;

use iris_core::library::Kind;
use iris_core::player::{Command, SessionEvent, TrackKind};
use iris_core::settings::{OnEof, PhotoDefault, Settings};

use super::ports::{
    EofOutcome, NowPlayingSnapshot, PlaybackDomainChanges, PlaybackEventReport,
    PlaybackSessionPort, SubtitleFlushOutcome, track_label,
};
use super::state::{
    NOW_PLAYING_MIN_INTERVAL, PlaybackOrchestrator, STATS_REFRESH_INTERVAL, VOLUME_ANNOUNCE_WINDOW,
    compute_now_playing, should_prevent_display_sleep, should_recover_after_gpu_reset,
};

impl<S: PlaybackSessionPort> PlaybackOrchestrator<S> {
    /// Applies a batch of session updates and returns pure domain changes, target
    /// sleep-preventer / NowPlaying updates, EOF outcome, and deferred OSD messages.
    pub fn process_session_updates(
        &mut self,
        batch: Vec<S::Update>,
        media_load_pending: bool,
        scoped_released: bool,
        on_eof: OnEof,
        now: Instant,
    ) -> PlaybackEventReport {
        let batch = S::coalesce_updates(batch);
        let was_ended = self.session.state().ended;
        let mut changes = PlaybackDomainChanges::default();
        let mut saw_position: Option<f64> = None;
        let mut clock_changed = false;
        let mut saw_ended_now = false;
        let mut saw_cache_duration = false;
        let mut last_error: Option<i32> = None;

        for update in &batch {
            let Some(ev) = self.session.apply_update(update) else {
                continue;
            };
            clock_changed |= matches!(
                ev,
                SessionEvent::Position(_)
                    | SessionEvent::Duration(_)
                    | SessionEvent::Paused(_)
                    | SessionEvent::Speed(_)
                    | SessionEvent::Buffering(_)
                    | SessionEvent::Ended(_)
                    | SessionEvent::Error(_)
                    | SessionEvent::Loaded
            );
            match ev {
                SessionEvent::Seek => {
                    changes.seeked = true;
                    clock_changed = true;
                }
                SessionEvent::Position(t) => saw_position = Some(*t),
                SessionEvent::Duration(_) => changes.duration_changed = true,
                SessionEvent::Paused(_) | SessionEvent::Speed(_) => {
                    changes.pause_or_speed_changed = true;
                }
                SessionEvent::Ended(true) => saw_ended_now = true,
                SessionEvent::Audio(_) => changes.audio_track_changed = true,
                SessionEvent::SubtitleTrack(_) => changes.subtitle_track_changed = true,
                SessionEvent::Volume(_) => changes.volume_changed = true,
                SessionEvent::Buffering(_) => changes.buffering_changed = true,
                SessionEvent::Loaded | SessionEvent::Reconfigured => {
                    changes.loaded_or_reconfigured = true;
                }
                SessionEvent::CacheDuration(_) => saw_cache_duration = true,
                SessionEvent::Error(code) => last_error = Some(*code),
                _ => changes.other_session_changed = true,
            }
        }
        self.session.flush_deferred();

        let kind = self.session.kind();
        changes.media_kind = kind;

        let should_flush_subtitles =
            changes.loaded_or_reconfigured && self.session.state().loaded && !media_load_pending;
        let sleep_prevent_active = should_prevent_display_sleep(kind, self.session.state());

        let (duration, paused, ended, loop_file, cache_end) = {
            let state = self.session.state();
            let buffer = (!state.is_live && state.cache_duration > 0.)
                .then_some(state.time_pos + state.cache_duration);
            (
                state.duration,
                state.paused,
                state.ended,
                state.loop_file,
                buffer,
            )
        };

        let force_now_playing = changes.pause_or_speed_changed
            || changes.loaded_or_reconfigured
            || was_ended != ended
            || (paused && (saw_position.is_some() || changes.duration_changed));
        let mut now_playing_updated =
            if force_now_playing || saw_position.is_some() || changes.duration_changed {
                self.poll_now_playing(force_now_playing, now)
            } else {
                None
            };

        let mut eof_outcome = EofOutcome::None;
        if saw_ended_now && !was_ended && !media_load_pending && !scoped_released && ended {
            if matches!(kind, Some(Kind::Video | Kind::Audio))
                && on_eof == OnEof::Next
                && !loop_file
            {
                eof_outcome = EofOutcome::SaveAndStepNext;
            } else {
                changes.ended_settled = true;
                eof_outcome = EofOutcome::SaveAndSettle;
            }
        }

        let mut osd_messages = Vec::new();
        if changes.audio_track_changed && self.announce_audio {
            self.announce_audio = false;
            let label = self
                .session
                .selected_track(TrackKind::Audio)
                .map_or_else(|| "无音轨".into(), track_label);
            osd_messages.push(format!("音轨 · {label}"));
        }
        if changes.subtitle_track_changed && self.announce_sub {
            self.announce_sub = false;
            let label = self
                .session
                .selected_track(TrackKind::Sub)
                .map_or_else(|| "无字幕".into(), track_label);
            osd_messages.push(format!("字幕 · {label}"));
        }
        if changes.volume_changed
            && self
                .announce_volume
                .take()
                .is_some_and(|req| now.saturating_duration_since(req) < VOLUME_ANNOUNCE_WINDOW)
        {
            let volume = self.session.state().volume;
            osd_messages.push(format!("音量 · {volume:.0}%"));
        }

        let mut error_code = None;
        if let Some(code) = last_error
            && !media_load_pending
            && !scoped_released
        {
            self.pending_subtitles.clear();
            now_playing_updated = self.poll_now_playing(true, now);
            changes.error_surfaced = true;
            error_code = Some(code);
        }

        PlaybackEventReport {
            changes,
            position: saw_position,
            cache_duration_changed: saw_cache_duration,
            duration,
            paused,
            ended,
            cache_end,
            clock_changed,
            should_flush_subtitles,
            sleep_prevent_active,
            now_playing_updated,
            eof_outcome,
            osd_messages,
            error_code,
        }
    }

    /// Starts a local path or multi-file queue preparation, advancing `load_generation`.
    pub fn begin_local_load(&mut self, pending_path: Option<PathBuf>) -> u64 {
        self.load_generation += 1;
        self.pending_path = pending_path;
        self.loading_visible = false;
        self.load_generation
    }

    /// Starts a direct remote HTTP/HTTPS load, immediately marking `loading_visible` true.
    pub fn begin_remote_load(&mut self) -> u64 {
        self.load_generation += 1;
        self.pending_path = None;
        self.loading_visible = true;
        self.load_generation
    }

    /// Starts an asynchronous plugin stream resolve, clearing `pending_path` and deferred subtitles.
    pub fn begin_plugin_resolve(&mut self) -> u64 {
        self.load_generation += 1;
        self.pending_path = None;
        self.loading_visible = true;
        self.pending_subtitles.clear();
        self.load_generation
    }

    /// Cancels any pending local/remote/plugin media load state and invalidates its `load_generation`.
    pub fn cancel_in_flight_load(&mut self, had_external_load: bool) -> bool {
        let was_local = self.pending_path.take().is_some();
        if was_local || had_external_load {
            self.load_generation += 1;
            self.loading_visible = false;
            self.pending_subtitles.clear();
            return true;
        }
        false
    }

    /// Reveals the delayed 150ms loading pill only if `generation` is still the active local load.
    pub fn maybe_show_loading_indicator(&mut self, generation: u64) -> bool {
        if self.load_generation == generation && self.pending_path.is_some() {
            self.loading_visible = true;
            return true;
        }
        false
    }

    /// Validates an async media preparation result against `generation`, advancing `PendingSubtitles`
    /// onto the committed open generation and clearing `pending_path` / `loading_visible`.
    pub fn accept_prepared_load(&mut self, generation: u64) -> bool {
        if self.load_generation != generation {
            return false;
        }
        self.pending_path = None;
        self.commit_media_open_generation();
        true
    }

    /// Commits a synchronous media open (e.g. admitted plugin `ScopedPlayback`), advancing
    /// `load_generation` and carrying forward any matching `PendingSubtitles`.
    pub fn commit_media_open_generation(&mut self) -> u64 {
        self.pending_path = None;
        let prev = self.load_generation;
        self.load_generation += 1;
        self.pending_subtitles.advance(prev, self.load_generation);
        self.loading_visible = false;
        self.load_generation
    }

    /// Resets per-media cached stats and arms `pending_photo_actual` when a new media open succeeds.
    pub fn on_media_open_succeeded(&mut self, photo_default: PhotoDefault) {
        self.pending_photo_actual =
            self.session.kind() == Some(Kind::Photo) && photo_default == PhotoDefault::Actual;
        self.cached_stats = None;
    }

    /// Clears deferred subtitles and forces a NowPlaying refresh after a failed media open.
    pub fn on_media_open_failed(&mut self, now: Instant) -> Option<Option<NowPlayingSnapshot>> {
        self.loading_visible = false;
        self.pending_subtitles.clear();
        self.poll_now_playing(true, now)
    }

    /// Attaches external subtitle sidecars to the current `load_generation` and reports whether
    /// they can be flushed immediately.
    pub fn enqueue_subtitles(&mut self, subtitles: Vec<PathBuf>, media_load_pending: bool) -> bool {
        if subtitles.is_empty() {
            return false;
        }
        self.pending_subtitles.add(self.load_generation, subtitles);
        !media_load_pending && (self.session.state().loaded || self.session.kind().is_none())
    }

    /// Flushes any `PendingSubtitles` matching the current `load_generation` into `Session`.
    pub fn flush_pending_subtitles(&mut self) -> SubtitleFlushOutcome {
        let paths = self.pending_subtitles.take(self.load_generation);
        if paths.is_empty() {
            return SubtitleFlushOutcome::Empty;
        }
        if !matches!(self.session.kind(), Some(Kind::Video | Kind::Audio)) {
            return SubtitleFlushOutcome::MediaNotReady;
        }
        let count = paths.len();
        let mut failed = 0;
        let mut last_error = None;
        for path in paths {
            let result = std::path::absolute(&path)
                .map_err(anyhow::Error::from)
                .and_then(|local| self.session.execute(Command::LoadSubtitle(local)));
            if let Err(error) = result {
                failed += 1;
                last_error = Some(error);
            }
        }
        if let Some(last_error) = last_error {
            SubtitleFlushOutcome::PartialFailure {
                count,
                failed,
                last_error,
            }
        } else {
            SubtitleFlushOutcome::Loaded { count }
        }
    }

    /// Refreshes full or dynamic `MediaStats` and updates `last_stats_refresh`.
    pub fn refresh_cached_stats(&mut self, full: bool, now: Instant) {
        if full || self.cached_stats.is_none() {
            self.cached_stats = Some(self.session.media_stats());
        } else if let Some(stats) = &mut self.cached_stats {
            self.session.refresh_dynamic_stats(stats);
        }
        self.last_stats_refresh = now;
    }

    /// Refreshes dynamic `MediaStats` at most once per [`STATS_REFRESH_INTERVAL`] (`1s`).
    pub fn refresh_stats_if_due(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_stats_refresh) >= STATS_REFRESH_INTERVAL {
            self.refresh_cached_stats(false, now);
            return true;
        }
        false
    }

    /// Throttles NowPlaying updates to `2s` unless `force` is set.
    pub fn poll_now_playing(
        &mut self,
        force: bool,
        now: Instant,
    ) -> Option<Option<NowPlayingSnapshot>> {
        if !force && now.saturating_duration_since(self.last_now_playing) < NOW_PLAYING_MIN_INTERVAL
        {
            return None;
        }
        self.last_now_playing = now;
        Some(compute_now_playing(&self.session))
    }

    /// Evaluates [`should_recover_after_gpu_reset`] and triggers `Session::recover_unloaded_after_gpu_reset`
    /// when the reset generation still matches and the active video track was not already restored.
    pub fn recover_after_gpu_reset(
        &mut self,
        reset_gen: u64,
        restored_vid: bool,
        settings: &Settings,
        zoom: f64,
        pan: [f64; 2],
    ) -> bool {
        if !should_recover_after_gpu_reset(
            reset_gen,
            self.load_generation,
            self.session.kind(),
            self.session.state().loaded,
            restored_vid,
        ) {
            return false;
        }
        self.session
            .recover_unloaded_after_gpu_reset(settings, zoom, pan);
        true
    }
}
