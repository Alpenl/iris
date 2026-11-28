//! User and media-key playback commands on [`super::PlaybackOrchestrator`]:
//! A-B loop, play/pause, seek, frame step, volume/mute, speed, loop, subtitle
//! visibility/track/cue/delay, audio track, chapter step, aspect, rotate, and flip.

use std::time::Instant;

use iris_core::player::{Command, TrackKind, format_time, step_speed};

use super::ports::{
    MediaKeyInput, MediaKeyOutcome, PlaybackActionOutcome, PlaybackSessionPort, aspect_label,
};
use super::state::PlaybackOrchestrator;

impl<S: PlaybackSessionPort> PlaybackOrchestrator<S> {
    pub fn execute_command(&mut self, command: Command) -> PlaybackActionOutcome {
        self.execute_with_osd(command, None)
    }

    fn execute_with_osd(&mut self, command: Command, osd: Option<String>) -> PlaybackActionOutcome {
        match self.session.execute(command) {
            Ok(()) => PlaybackActionOutcome {
                osd,
                ..Default::default()
            },
            Err(error) => PlaybackActionOutcome {
                osd,
                error: Some(error),
                ..Default::default()
            },
        }
    }

    pub fn cycle_ab_loop(&mut self) -> PlaybackActionOutcome {
        self.execute_with_osd(Command::CycleAbLoop, None)
    }

    pub fn clear_ab_loop(&mut self) -> PlaybackActionOutcome {
        self.execute_with_osd(Command::ClearAbLoop, None)
    }

    pub fn toggle_pause(&mut self) -> PlaybackActionOutcome {
        let st = self.session.state();
        let next_paused = !st.paused && !st.ended;
        let label = if next_paused { "暂停" } else { "播放" };
        self.execute_with_osd(Command::TogglePlayback, Some(label.into()))
    }

    pub fn seek_relative_step(&mut self, step: f64, forward: bool) -> PlaybackActionOutcome {
        let st = self.session.state();
        let (delta, prefix, target) = if forward {
            let target = if st.duration > 0. {
                (st.time_pos + step).min(st.duration)
            } else {
                st.time_pos + step
            };
            (step, "+", target)
        } else {
            (-step, "-", (st.time_pos - step).max(0.))
        };
        let osd = format!("{prefix}{step:.0}s · {}", format_time(target));
        self.execute_with_osd(Command::SeekRelative(delta), Some(osd))
    }

    pub fn seek_relative_large(&mut self, base_step: f64, forward: bool) -> PlaybackActionOutcome {
        let step = (base_step * 6.).clamp(15., 120.);
        self.seek_relative_step(step, forward)
    }

    pub fn frame_step(&mut self, forward: bool) -> PlaybackActionOutcome {
        let (cmd, label) = if forward {
            (Command::FrameStep, "下一帧")
        } else {
            (Command::FrameBackStep, "上一帧")
        };
        self.execute_with_osd(cmd, Some(label.into()))
    }

    pub fn adjust_volume_by(&mut self, delta: f64, now: Instant) -> Result<(), anyhow::Error> {
        self.session.execute(Command::VolumeBy(delta))?;
        self.announce_volume = Some(now);
        Ok(())
    }

    pub fn toggle_mute(&mut self) -> PlaybackActionOutcome {
        let next = !self.session.state().muted;
        let label = if next { "已静音" } else { "已取消静音" };
        self.execute_with_osd(Command::ToggleMute, Some(label.into()))
    }

    pub fn step_speed(&mut self, direction: isize) -> PlaybackActionOutcome {
        let next = step_speed(self.session.state().speed, direction);
        self.execute_with_osd(Command::SetSpeed(next), Some(format!("倍速 · {next:.2}×")))
    }

    pub fn reset_speed(&mut self) -> PlaybackActionOutcome {
        self.execute_with_osd(Command::SetSpeed(1.), Some("倍速 · 1.00×".into()))
    }

    pub fn toggle_loop(&mut self) -> PlaybackActionOutcome {
        let next = !self.session.state().loop_file;
        let label = if next {
            "单曲循环已开启"
        } else {
            "单曲循环已关闭"
        };
        self.execute_with_osd(Command::SetLoop(next), Some(label.into()))
    }

    pub fn toggle_subtitles(&mut self) -> PlaybackActionOutcome {
        let next = !self.session.state().subs_visible;
        let label = if next {
            "字幕已显示"
        } else {
            "字幕已隐藏"
        };
        self.execute_with_osd(Command::ToggleSubtitles, Some(label.into()))
    }

    pub fn cycle_subtitle_track(&mut self) -> PlaybackActionOutcome {
        if self.session.tracks_of(TrackKind::Sub).is_empty() {
            return PlaybackActionOutcome {
                osd: Some("当前视频没有字幕轨".into()),
                ..Default::default()
            };
        }
        self.announce_sub = true;
        self.execute_with_osd(Command::NextSubtitle, None)
    }

    pub fn seek_subtitle_cue(&mut self, next: bool) -> PlaybackActionOutcome {
        if self.session.selected_track(TrackKind::Sub).is_none() {
            return PlaybackActionOutcome {
                osd: Some("请先选择字幕轨道".into()),
                ..Default::default()
            };
        }
        let cmd = if next {
            Command::SubSeekNext
        } else {
            Command::SubSeekPrev
        };
        self.execute_with_osd(cmd, None)
    }

    pub fn adjust_sub_delay(&mut self, delta_sec: f64) -> PlaybackActionOutcome {
        let next = ((self.session.state().sub_delay + delta_sec) * 100.).round() / 100.;
        self.execute_with_osd(
            Command::SetSubDelay(next),
            Some(format!("字幕偏移 · {next:+.1}s")),
        )
    }

    pub fn reset_sub_delay(&mut self) -> PlaybackActionOutcome {
        self.execute_with_osd(Command::SetSubDelay(0.), Some("字幕偏移 · 0.0s".into()))
    }

    pub fn cycle_audio_track(&mut self) -> PlaybackActionOutcome {
        if self.session.tracks_of(TrackKind::Audio).len() <= 1 {
            return PlaybackActionOutcome {
                osd: Some("当前视频没有其他音轨".into()),
                ..Default::default()
            };
        }
        self.announce_audio = true;
        self.execute_with_osd(Command::NextAudio, None)
    }

    pub fn step_chapter(&mut self, next: bool) -> PlaybackActionOutcome {
        if self.session.state().chapters.is_empty() {
            return PlaybackActionOutcome {
                osd: Some("当前视频没有章节".into()),
                ..Default::default()
            };
        }
        let (cmd, label) = if next {
            (Command::NextChapter, "下一章节")
        } else {
            (Command::PrevChapter, "上一章节")
        };
        self.execute_with_osd(cmd, Some(label.into()))
    }

    pub fn cycle_aspect(&mut self) -> PlaybackActionOutcome {
        let next = self.session.state().aspect.next();
        self.execute_with_osd(
            Command::SetAspect(next),
            Some(format!("画面比例 · {}", aspect_label(next))),
        )
    }

    pub fn rotate_media(&mut self, degrees: i64, label: &'static str) -> PlaybackActionOutcome {
        match self.session.execute(Command::RotateMedia(degrees)) {
            Ok(()) => PlaybackActionOutcome {
                osd: Some(label.into()),
                reset_image_view: true,
                ..Default::default()
            },
            Err(error) => PlaybackActionOutcome {
                error: Some(error),
                ..Default::default()
            },
        }
    }

    pub fn flip_display_axis(&mut self, horizontal: bool) -> PlaybackActionOutcome {
        let command = if horizontal {
            Command::ToggleHFlip
        } else {
            Command::ToggleVFlip
        };
        if let Err(error) = self.session.execute(command) {
            return PlaybackActionOutcome {
                error: Some(error),
                ..Default::default()
            };
        }
        let source_horizontal = horizontal ^ (self.session.display_rotation() % 180 == 90);
        let state = self.session.state();
        let on = if source_horizontal {
            state.hflip
        } else {
            state.vflip
        };
        let label = match (horizontal, on) {
            (true, true) => "水平镜像",
            (true, false) => "还原水平镜像",
            (false, true) => "垂直镜像",
            (false, false) => "还原垂直镜像",
        };
        PlaybackActionOutcome {
            osd: Some(label.into()),
            flip_pan_axis: Some(usize::from(!horizontal)),
            ..Default::default()
        }
    }

    /// Resolves a platform [`MediaKeyInput`] against current session state.
    pub fn handle_media_key(&mut self, key: MediaKeyInput) -> MediaKeyOutcome {
        let state = self.session.state();
        let cmd = match key {
            MediaKeyInput::Play if state.paused || state.ended => Some(Command::TogglePlayback),
            MediaKeyInput::Play => None,
            MediaKeyInput::Pause if !state.paused && !state.ended => Some(Command::TogglePlayback),
            MediaKeyInput::Pause => None,
            MediaKeyInput::Toggle => Some(Command::TogglePlayback),
            MediaKeyInput::Next => return MediaKeyOutcome::StepQueue { delta: 1 },
            MediaKeyInput::Prev => return MediaKeyOutcome::StepQueue { delta: -1 },
            MediaKeyInput::Seek(seconds) => Some(Command::SeekAbsolute(seconds)),
        };
        let Some(command) = cmd else {
            return MediaKeyOutcome::Ignored;
        };
        match self.session.execute(command) {
            Ok(()) => MediaKeyOutcome::Executed,
            Err(err) => MediaKeyOutcome::Failed(err),
        }
    }
}
