use super::*;

impl Session {
    pub fn execute(&mut self, command: Command) -> Result<()> {
        if let Some(time) = self.plugin_chapter_seek(&command) {
            return self.execute(Command::SeekAbsolute(time));
        }
        if matches!(
            command,
            Command::SeekRelative(_)
                | Command::SeekAbsolute(_)
                | Command::SeekPercent(_)
                | Command::SeekPercentFast(_)
                | Command::FrameStep
                | Command::FrameBackStep
                | Command::SubSeekPrev
                | Command::SubSeekNext
                | Command::NextChapter
                | Command::PrevChapter
                | Command::SelectChapter(_)
        ) && let Some(playback) = &self.scoped_playback
        {
            playback.relay.session().advance_seek()?;
        }
        match command {
            Command::SetAbLoop { start, end } => {
                if !(self.state.loaded
                    && !self.state.is_live
                    && matches!(self.kind, Some(Kind::Video | Kind::Audio))
                    && start.is_finite()
                    && end.is_finite()
                    && start >= 0.
                    && start < end
                    && (self.state.duration <= 0. || end <= self.state.duration))
                {
                    return Err(PlayerActionError::InvalidAbRange.into());
                }
                self.mpv.command_async(&["set", "ab-loop-b", "no"])?;
                self.mpv.set_property_f64_async("ab-loop-a", start)?;
                self.mpv.set_property_f64_async("ab-loop-b", end)?;
                self.mpv.command_async(&["set", "ab-loop-count", "inf"])?;
            }
            Command::CycleAbLoop => {
                if !(self.state.loaded
                    && !self.state.is_live
                    && matches!(self.kind, Some(Kind::Video | Kind::Audio)))
                {
                    return Err(PlayerActionError::NotSeekable.into());
                }
                self.mpv.command_async(&["ab-loop"])?;
            }
            Command::ClearAbLoop => {
                self.mpv.command_async(&["set", "ab-loop-b", "no"])?;
                self.mpv.command_async(&["set", "ab-loop-a", "no"])?;
                self.state.ab_start = None;
                self.state.ab_end = None;
            }
            Command::TogglePlayback => {
                if self.state.ended {
                    self.mpv.command_async(&["seek", "0", "absolute+exact"])?;
                    self.mpv.command_async(&["set", "pause", "no"])?;
                } else {
                    self.mpv.command_async(&["cycle", "pause"])?;
                }
            }
            Command::SeekRelative(s) => {
                self.mpv
                    .command_async(&["seek", &s.to_string(), "relative+exact"])?;
            }
            Command::SeekAbsolute(s) => {
                self.mpv
                    .command_async(&["seek", &s.max(0.).to_string(), "absolute+exact"])?;
            }
            Command::SeekPercent(p) => {
                self.mpv.command_async(&[
                    "seek",
                    &p.clamp(0., 100.).to_string(),
                    "absolute-percent+exact",
                ])?;
            }
            Command::SeekPercentFast(p) => {
                self.mpv.command_async(&[
                    "seek",
                    &p.clamp(0., 100.).to_string(),
                    "absolute-percent+keyframes",
                ])?;
            }
            Command::FrameStep => self.mpv.command_async(&["frame-step"])?,
            Command::FrameBackStep => self.mpv.command_async(&["frame-back-step"])?,
            Command::ToggleMute => self.mpv.command_async(&["cycle", "mute"])?,
            Command::VolumeBy(delta) => {
                if delta.is_finite() && delta != 0. {
                    self.mpv.command(&["add", "volume", &delta.to_string()])?;
                    if self
                        .mpv
                        .get_property_f64("volume")
                        .is_ok_and(|volume| volume > 0.)
                    {
                        self.mpv.command_async(&["set", "mute", "no"])?;
                    }
                }
            }
            Command::SetVolume(volume) => {
                let clamped = volume.clamp(0., 100.);
                if clamped > 0. && self.state.muted {
                    self.mpv.command_async(&["set", "mute", "no"])?;
                }
                self.mpv.set_property_f64_async("volume", clamped)?;
            }
            Command::SetSpeed(s) => self
                .mpv
                .set_property_f64_async("speed", s.clamp(MIN_SPEED, MAX_SPEED))?,
            Command::ToggleSubtitles => self.mpv.command_async(&["cycle", "sub-visibility"])?,
            Command::NextSubtitle => {
                if !self.state.subs_visible {
                    self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
                }
                self.mpv.command_async(&["cycle", "sub"])?;
            }
            Command::SelectSubtitle(Some(id)) => {
                self.mpv.command_async(&["set", "sid", &id.to_string()])?;
                self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
            }
            Command::SelectSubtitle(None) => {
                self.mpv.command_async(&["set", "sub-visibility", "no"])?;
            }
            Command::SubSeekPrev => self.mpv.command_async(&["sub-seek", "-1"])?,
            Command::SubSeekNext => self.mpv.command_async(&["sub-seek", "1"])?,
            Command::SubDelayBy(delta) => {
                let next = ((self.state.sub_delay + delta) * 100.).round() / 100.;
                self.mpv
                    .set_property_f64_async("sub-delay", next.clamp(-60., 60.))?;
            }
            Command::SetSubDelay(d) => {
                self.mpv
                    .set_property_f64_async("sub-delay", d.clamp(-60., 60.))?;
            }
            Command::LoadSubtitle(path) => {
                let raw_in = path.to_string_lossy();
                if raw_in
                    .split_once("://")
                    .is_some_and(|(scheme, _)| {
                        !scheme.is_empty()
                            && scheme
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
                    })
                {
                    return Err(PlayerActionError::InvalidSubtitlePath.into());
                }
                let resolved = crate::library::safe_resolve_path(&path);
                if !(resolved.is_absolute()
                    && std::fs::metadata(&resolved).is_ok_and(|m| m.is_file()))
                {
                    return Err(PlayerActionError::SubtitleFileNotFound.into());
                }
                if self.scoped_playback.is_none() {
                    let _ = super::references::disable_hls(&self.mpv);
                }
                let target = resolved.to_string_lossy();
                self.mpv.command_async(&["sub-add", &target, "select"])?;
                self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
            }
            Command::NextAudio => self.mpv.command_async(&["cycle", "audio"])?,
            Command::SelectAudio(Some(id)) => {
                self.mpv.command_async(&["set", "aid", &id.to_string()])?;
            }
            Command::SelectAudio(None) => self.mpv.command_async(&["set", "aid", "no"])?,
            Command::SetAudioDevice(dev) => {
                let clean = if dev.trim().is_empty() || dev.chars().any(char::is_control) {
                    "auto".into()
                } else {
                    dev.trim().to_string()
                };
                self.mpv.command_async(&["set", "audio-device", &clean])?;
                self.state.audio_device = clean;
            }
            Command::SetLoop(enable) => {
                self.mpv.command_async(&[
                    "set",
                    "loop-file",
                    if enable && !self.state.is_live {
                        "inf"
                    } else {
                        "no"
                    },
                ])?;
            }
            Command::SetAspect(mode) => {
                self.state.aspect = mode;
                let (aspect_str, panscan) = match mode {
                    AspectMode::Original => ("no", 0.),
                    AspectMode::Ratio16x9 => ("16:9", 0.),
                    AspectMode::Ratio4x3 => ("4:3", 0.),
                    AspectMode::Ratio21x9 => ("21:9", 0.),
                    AspectMode::FillCrop => ("no", 1.),
                };
                self.mpv
                    .command_async(&["set", "video-aspect-override", aspect_str])?;
                self.mpv.set_property_f64_async("panscan", panscan)?;
            }
            Command::SetVideoEq {
                brightness,
                contrast,
                saturation,
                gamma,
            } => {
                let (b, c, s, g) = (
                    brightness.clamp(-100, 100),
                    contrast.clamp(-100, 100),
                    saturation.clamp(-100, 100),
                    gamma.clamp(-100, 100),
                );
                self.mpv.set_property_f64_async("brightness", b as f64)?;
                self.mpv.set_property_f64_async("contrast", c as f64)?;
                self.mpv.set_property_f64_async("saturation", s as f64)?;
                self.mpv.set_property_f64_async("gamma", g as f64)?;
                self.state.eq_brightness = b;
                self.state.eq_contrast = c;
                self.state.eq_saturation = s;
                self.state.eq_gamma = g;
            }
            Command::ResetVideoEq => {
                for prop in ["brightness", "contrast", "saturation", "gamma"] {
                    self.mpv.set_property_f64_async(prop, 0.)?;
                }
                self.state.eq_brightness = 0;
                self.state.eq_contrast = 0;
                self.state.eq_saturation = 0;
                self.state.eq_gamma = 0;
            }
            Command::RotateMedia(delta_deg) => {
                if !matches!(self.kind, Some(Kind::Photo | Kind::Video)) {
                    return Ok(());
                }
                self.extra_rotate = ((self.extra_rotate as i64 + delta_deg).rem_euclid(360)) as u16;
                if self.uses_filter_orientation() {
                    self.apply_vf_filters()?;
                } else {
                    self.mpv.command_async(&[
                        "set",
                        "video-rotate",
                        &self.extra_rotate.to_string(),
                    ])?;
                }
            }
            command @ (Command::ToggleHFlip | Command::ToggleVFlip) => {
                let horizontal = matches!(command, Command::ToggleHFlip);
                if horizontal != self.display_axes_swapped() {
                    self.state.hflip = !self.state.hflip;
                } else {
                    self.state.vflip = !self.state.vflip;
                }
                self.apply_vf_filters()?;
            }
            Command::NextChapter => self.mpv.command_async(&["add", "chapter", "1"])?,
            Command::PrevChapter => self.mpv.command_async(&["add", "chapter", "-1"])?,
            Command::SelectChapter(ix) => {
                self.mpv
                    .command_async(&["set", "chapter", &ix.to_string()])?;
            }
            Command::Screenshot { path, with_subs } => {
                let target = path.to_string_lossy();
                let flags = if with_subs { "subtitles" } else { "video" };
                self.mpv
                    .command_async(&["screenshot-to-file", &target, flags])?;
            }
        }
        Ok(())
    }
}
