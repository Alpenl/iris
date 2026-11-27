use anyhow::Result;

use crate::library::Kind;

use super::{Command, PlayerActionError, Session};

impl Session {
    pub fn execute(&mut self, command: Command) -> Result<()> {
        if let Some(time) = self.plugin_chapter_seek(&command) {
            return self.execute(Command::SeekAbsolute(time));
        }
        if (matches!(
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
        ) || (matches!(command, Command::TogglePlayback) && self.machine.state.ended))
            && let Some(playback) = self.machine.source.scoped_playback()
        {
            playback.stream.advance_seek()?;
        }

        if let Some(pure_res) = self.machine.reduce_pure_command(&command) {
            let ops = pure_res?;
            return self.run_ops(ops);
        }

        match command {
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
            Command::LoadSubtitle(path) => {
                let raw_in = super::super::mpv_path(&path)?;
                if raw_in.split_once("://").is_some_and(|(scheme, _)| {
                    !scheme.is_empty()
                        && scheme
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
                }) {
                    return Err(PlayerActionError::InvalidSubtitlePath.into());
                }
                let resolved = crate::library::safe_resolve_path(&path);
                if !(resolved.is_absolute()
                    && std::fs::metadata(&resolved).is_ok_and(|m| m.is_file()))
                {
                    return Err(PlayerActionError::SubtitleFileNotFound.into());
                }
                let ops = self.machine.queue_or_emit_local_subtitle(
                    &resolved,
                    self.enhancements.tracks.has_pending_adds(),
                    self.enhancements.tracks.has_any_tracks(),
                )?;
                self.run_ops(ops)?;
            }
            Command::RotateMedia(delta_deg) => {
                if !matches!(self.kind(), Some(Kind::Photo | Kind::Video)) {
                    return Ok(());
                }
                self.visual.extra_rotate =
                    ((self.visual.extra_rotate as i64 + delta_deg).rem_euclid(360)) as u16;
                if self.uses_filter_orientation() {
                    self.apply_vf_filters()?;
                } else {
                    self.mpv.command_async(&[
                        "set",
                        "video-rotate",
                        &self.visual.extra_rotate.to_string(),
                    ])?;
                }
            }
            command @ (Command::ToggleHFlip | Command::ToggleVFlip) => {
                let horizontal = matches!(command, Command::ToggleHFlip);
                if horizontal != self.display_axes_swapped() {
                    self.machine.state.hflip = !self.machine.state.hflip;
                } else {
                    self.machine.state.vflip = !self.machine.state.vflip;
                }
                self.apply_vf_filters()?;
            }
            _ => unreachable!("handled by PlayerMachine::reduce_pure_command"),
        }
        Ok(())
    }
}
