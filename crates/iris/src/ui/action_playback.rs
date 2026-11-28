//! Playback, seek, volume, speed, track, chapter, and queue-step GPUI action
//! adapters delegating all orchestration to [`iris_app::AppModel`].

use std::time::Instant;

use gpui::{Context, Window};
use iris_app::PlaybackActionOutcome;

use super::WindowShell;
use super::actions::{
    ClearAbLoop, CycleAbLoop, CycleAudio, CycleSub, FrameBack, FrameForward, NextChapter,
    OpenNextMedia, OpenPreviousMedia, PrevChapter, ResetSpeed, SeekBackwardExact,
    SeekBackwardLarge, SeekForwardExact, SeekForwardLarge, SpeedDown, SpeedUp, SubDelayBack,
    SubDelayForward, SubDelayReset, SubSeekNext, SubSeekPrev, ToggleLoop, ToggleMute, TogglePause,
    ToggleSubs, VolumeDown, VolumeUp,
};

impl WindowShell {
    pub(in crate::ui) fn apply_playback_action(
        &mut self,
        outcome: PlaybackActionOutcome,
        cx: &mut Context<Self>,
    ) {
        if let Some(error) = outcome.error {
            log::warn!("player command failed: {error:#}");
            self.show_error_toast(super::strings::errors::player_action_error(&error), cx);
            return;
        }
        if outcome.reset_image_view {
            self.active_mut().image.reset();
            self.apply_image_view(cx);
        } else if let Some(axis) = outcome.flip_pan_axis {
            self.active_mut().image.pan[axis] *= -1.;
            self.apply_image_view(cx);
        }
        if let Some(osd) = outcome.osd {
            self.show_osd(osd, cx);
        }
    }

    pub(super) fn cycle_ab_loop(
        &mut self,
        _: &CycleAbLoop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.cycle_ab_loop();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn clear_ab_loop(
        &mut self,
        _: &ClearAbLoop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.clear_ab_loop();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.toggle_pause();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn seek_back(
        &mut self,
        _: &SeekBackwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_relative_step(false);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn seek_forward(
        &mut self,
        _: &SeekForwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_relative_step(true);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn seek_back_large(
        &mut self,
        _: &SeekBackwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_relative_large(false);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn seek_forward_large(
        &mut self,
        _: &SeekForwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_relative_large(true);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn frame_back(&mut self, _: &FrameBack, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.frame_step(false);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn frame_forward(
        &mut self,
        _: &FrameForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.frame_step(true);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_volume_by(5., cx);
    }

    pub(super) fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.adjust_volume_by(-5., cx);
    }

    pub(super) fn adjust_volume_by(&mut self, delta: f64, cx: &mut Context<Self>) {
        if let Err(error) = self.app.adjust_volume_by(delta, Instant::now()) {
            self.show_toast(format!("操作失败 · {error}"), cx);
        }
    }

    pub(super) fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.toggle_mute();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.step_speed(-1);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.step_speed(1);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn reset_speed(&mut self, _: &ResetSpeed, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.reset_speed();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_loop(&mut self, _: &ToggleLoop, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.toggle_loop();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.toggle_subtitles();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.cycle_subtitle_track();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn sub_seek_prev(
        &mut self,
        _: &SubSeekPrev,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_subtitle_cue(false);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn sub_seek_next(
        &mut self,
        _: &SubSeekNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.seek_subtitle_cue(true);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn sub_delay_back(
        &mut self,
        _: &SubDelayBack,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.adjust_sub_delay(-0.1);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn sub_delay_forward(
        &mut self,
        _: &SubDelayForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.adjust_sub_delay(0.1);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn sub_delay_reset(
        &mut self,
        _: &SubDelayReset,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.app.reset_sub_delay();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.cycle_audio_track();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn prev_chapter(&mut self, _: &PrevChapter, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.step_chapter(false);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn next_chapter(&mut self, _: &NextChapter, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.step_chapter(true);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn next_item(&mut self, _: &OpenNextMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, true, cx);
    }

    pub(super) fn prev_item(
        &mut self,
        _: &OpenPreviousMedia,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(-1, true, cx);
    }
}
