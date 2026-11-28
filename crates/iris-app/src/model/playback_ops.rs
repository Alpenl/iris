//! Playback transport, seek, volume, speed, track, chapter, transform,
//! media-key, GPU-reset recovery, and session event batch forwarding on [`super::AppModel`].

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use iris_core::player::Command;

use super::AppModel;
use crate::playback::{
    MediaKeyInput, MediaKeyOutcome, NowPlayingSnapshot, PlaybackActionOutcome, PlaybackEventReport,
    PlaybackSessionPort, SubtitleFlushOutcome,
};
use crate::plugins::PluginPlayer;

impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    pub fn execute_player_command(&mut self, cmd: Command) -> PlaybackActionOutcome {
        self.playback.execute_command(cmd)
    }

    pub fn cycle_ab_loop(&mut self) -> PlaybackActionOutcome {
        self.playback.cycle_ab_loop()
    }

    pub fn clear_ab_loop(&mut self) -> PlaybackActionOutcome {
        self.playback.clear_ab_loop()
    }

    pub fn toggle_pause(&mut self) -> PlaybackActionOutcome {
        self.playback.toggle_pause()
    }

    pub fn seek_relative_step(&mut self, forward: bool) -> PlaybackActionOutcome {
        self.playback
            .seek_relative_step(self.workbench.settings.seek_step, forward)
    }

    pub fn seek_relative_large(&mut self, forward: bool) -> PlaybackActionOutcome {
        self.playback
            .seek_relative_large(self.workbench.settings.seek_step, forward)
    }

    pub fn frame_step(&mut self, forward: bool) -> PlaybackActionOutcome {
        self.playback.frame_step(forward)
    }

    pub fn adjust_volume_by(&mut self, delta: f64, now: Instant) -> Result<()> {
        self.playback.adjust_volume_by(delta, now)
    }

    pub fn toggle_mute(&mut self) -> PlaybackActionOutcome {
        self.playback.toggle_mute()
    }

    pub fn step_speed(&mut self, direction: isize) -> PlaybackActionOutcome {
        self.playback.step_speed(direction)
    }

    pub fn reset_speed(&mut self) -> PlaybackActionOutcome {
        self.playback.reset_speed()
    }

    pub fn toggle_loop(&mut self) -> PlaybackActionOutcome {
        self.playback.toggle_loop()
    }

    pub fn toggle_subtitles(&mut self) -> PlaybackActionOutcome {
        self.playback.toggle_subtitles()
    }

    pub fn cycle_subtitle_track(&mut self) -> PlaybackActionOutcome {
        self.playback.cycle_subtitle_track()
    }

    pub fn seek_subtitle_cue(&mut self, forward: bool) -> PlaybackActionOutcome {
        self.playback.seek_subtitle_cue(forward)
    }

    pub fn adjust_sub_delay(&mut self, delta: f64) -> PlaybackActionOutcome {
        self.playback.adjust_sub_delay(delta)
    }

    pub fn reset_sub_delay(&mut self) -> PlaybackActionOutcome {
        self.playback.reset_sub_delay()
    }

    pub fn cycle_audio_track(&mut self) -> PlaybackActionOutcome {
        self.playback.cycle_audio_track()
    }

    pub fn step_chapter(&mut self, forward: bool) -> PlaybackActionOutcome {
        self.playback.step_chapter(forward)
    }

    pub fn cycle_aspect(&mut self) -> PlaybackActionOutcome {
        self.playback.cycle_aspect()
    }

    pub fn rotate_media(&mut self, degrees: i64, label: &'static str) -> PlaybackActionOutcome {
        self.playback.rotate_media(degrees, label)
    }

    pub fn flip_display_axis(&mut self, horizontal: bool) -> PlaybackActionOutcome {
        self.playback.flip_display_axis(horizontal)
    }

    pub fn seek_percent(&mut self, pct: f64, exact: bool) -> Result<()> {
        let cmd = if exact {
            Command::SeekPercent(pct)
        } else {
            Command::SeekPercentFast(pct)
        };
        PlaybackSessionPort::execute(self.playback.session_mut(), cmd)
    }

    pub fn transform_view(&mut self, zoom: f64, pan: [f64; 2]) -> Result<()> {
        self.playback.session_mut().transform(zoom, pan)
    }

    pub fn set_subtitle_inset(&mut self, logical_inset: f64, viewport_height: f64) {
        self.playback
            .session_mut()
            .subtitle_inset(logical_inset, viewport_height);
    }

    pub fn handle_media_key(&mut self, input: MediaKeyInput) -> MediaKeyOutcome {
        self.playback.handle_media_key(input)
    }

    pub fn recover_after_gpu_reset(
        &mut self,
        reset_gen: u64,
        restored_vid: bool,
        zoom: f64,
        pan: [f64; 2],
    ) -> bool {
        let settings = self.workbench.settings.clone();
        self.playback
            .recover_after_gpu_reset(reset_gen, restored_vid, &settings, zoom, pan)
    }

    pub fn process_session_updates(
        &mut self,
        batch: Vec<S::Update>,
        now: Instant,
    ) -> PlaybackEventReport {
        let media_load_pending = self.has_media_load_pending();
        self.playback.process_session_updates(
            batch,
            media_load_pending,
            self.plugins.scoped_released(),
            self.workbench.settings.on_eof,
            now,
        )
    }

    pub fn refresh_cached_stats(&mut self, force: bool, now: Instant) {
        self.playback.refresh_cached_stats(force, now);
    }

    pub fn refresh_stats_if_due(&mut self, now: Instant) -> bool {
        self.playback.refresh_stats_if_due(now)
    }

    pub fn take_pending_photo_actual(&mut self) -> bool {
        self.playback.take_pending_photo_actual()
    }

    pub fn enqueue_subtitles(&mut self, subtitles: Vec<PathBuf>, pending: bool) -> bool {
        self.playback.enqueue_subtitles(subtitles, pending)
    }

    pub fn flush_pending_subtitles(&mut self) -> SubtitleFlushOutcome {
        self.playback.flush_pending_subtitles()
    }

    pub fn maybe_show_loading_indicator(&mut self, generation: u64) -> bool {
        self.playback.maybe_show_loading_indicator(generation)
    }

    pub fn poll_now_playing(
        &mut self,
        force: bool,
        now: Instant,
    ) -> Option<Option<NowPlayingSnapshot>> {
        self.playback.poll_now_playing(force, now)
    }
}
