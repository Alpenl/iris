//! Playback, seek, volume, speed, track, chapter, and queue-step action handlers.

use gpui::{Context, Window};
use iris_core::player::{self, Command, TrackKind};

use super::IrisWindow;
use super::actions::{
    CycleAudio, CycleSub, FrameBack, FrameForward, NextChapter, OpenNextMedia, OpenPreviousMedia,
    PrevChapter, ResetSpeed, SeekBackwardExact, SeekBackwardLarge, SeekForwardExact,
    SeekForwardLarge, SpeedDown, SpeedUp, SubDelayBack, SubDelayForward, SubDelayReset,
    SubSeekNext, SubSeekPrev, ToggleLoop, ToggleMute, TogglePause, ToggleSubs, VolumeDown,
    VolumeUp,
};

impl IrisWindow {
    pub(super) fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        let st = self.active().session.state();
        let next_paused = !st.paused && !st.ended;
        self.command(Command::TogglePlayback, cx);
        self.show_osd(if next_paused { "暂停" } else { "播放" }, cx);
    }

    pub(super) fn seek_back(
        &mut self,
        _: &SeekBackwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self.settings.seek_step;
        let target = (self.active().session.state().time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(format!("-{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_forward(
        &mut self,
        _: &SeekForwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self.settings.seek_step;
        let st = self.active().session.state();
        let target = if st.duration > 0. {
            (st.time_pos + step).min(st.duration)
        } else {
            st.time_pos + step
        };
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(format!("+{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_back_large(
        &mut self,
        _: &SeekBackwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let target = (self.active().session.state().time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(format!("-{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_forward_large(
        &mut self,
        _: &SeekForwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let st = self.active().session.state();
        let target = if st.duration > 0. {
            (st.time_pos + step).min(st.duration)
        } else {
            st.time_pos + step
        };
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(format!("+{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn frame_back(&mut self, _: &FrameBack, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::FrameBackStep, cx);
        self.show_osd("上一帧", cx);
    }

    pub(super) fn frame_forward(
        &mut self,
        _: &FrameForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command(Command::FrameStep, cx);
        self.show_osd("下一帧", cx);
    }

    pub(super) fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active().session.state().volume + 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }

    pub(super) fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active().session.state().volume - 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }

    pub(super) fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state().muted;
        self.command(Command::ToggleMute, cx);
        self.show_osd(if next { "已静音" } else { "已取消静音" }, cx);
    }

    pub(super) fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.active().session.state().speed, -1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }

    pub(super) fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.active().session.state().speed, 1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }

    pub(super) fn reset_speed(&mut self, _: &ResetSpeed, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SetSpeed(1.), cx);
        self.show_osd("倍速 · 1.00×", cx);
    }

    pub(super) fn toggle_loop(&mut self, _: &ToggleLoop, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state().loop_file;
        self.command(Command::SetLoop(next), cx);
        self.show_osd(
            if next {
                "单曲循环已开启"
            } else {
                "单曲循环已关闭"
            },
            cx,
        );
    }

    pub(super) fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state().subs_visible;
        self.command(Command::ToggleSubtitles, cx);
        self.show_osd(
            if next {
                "字幕已显示"
            } else {
                "字幕已隐藏"
            },
            cx,
        );
    }

    pub(super) fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.tracks_of(TrackKind::Sub).is_empty() {
            self.show_osd("当前视频没有字幕轨", cx);
            return;
        }
        self.active_mut().announce_sub = true;
        self.command(Command::NextSubtitle, cx);
    }

    pub(super) fn sub_seek_prev(
        &mut self,
        _: &SubSeekPrev,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.seek_subtitle_cue(Command::SubSeekPrev, cx);
    }

    pub(super) fn sub_seek_next(
        &mut self,
        _: &SubSeekNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.seek_subtitle_cue(Command::SubSeekNext, cx);
    }

    fn seek_subtitle_cue(&mut self, command: Command, cx: &mut Context<Self>) {
        if self
            .active()
            .session
            .selected_track(TrackKind::Sub)
            .is_none()
        {
            self.show_osd("请先选择字幕轨道", cx);
            return;
        }
        self.command(command, cx);
    }

    pub(super) fn sub_delay_back(
        &mut self,
        _: &SubDelayBack,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = ((self.active().session.state().sub_delay - 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }

    pub(super) fn sub_delay_forward(
        &mut self,
        _: &SubDelayForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = ((self.active().session.state().sub_delay + 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }

    pub(super) fn sub_delay_reset(
        &mut self,
        _: &SubDelayReset,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command(Command::SetSubDelay(0.), cx);
        self.show_osd("字幕偏移 · 0.0s", cx);
    }

    pub(super) fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.tracks_of(TrackKind::Audio).len() <= 1 {
            self.show_osd("当前视频没有其他音轨", cx);
            return;
        }
        self.active_mut().announce_audio = true;
        self.command(Command::NextAudio, cx);
    }

    pub(super) fn prev_chapter(&mut self, _: &PrevChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.state().chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::PrevChapter, cx);
        self.show_osd("上一章节", cx);
    }

    pub(super) fn next_chapter(&mut self, _: &NextChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.state().chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::NextChapter, cx);
        self.show_osd("下一章节", cx);
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
