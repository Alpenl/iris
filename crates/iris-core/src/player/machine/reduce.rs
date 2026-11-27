//! Pure event/command reducers, local subtitle deferral, and update coalescing
//! for [`PlayerMachine`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, ensure};

use crate::library::Kind;
use crate::settings::{MAX_SPEED, MIN_SPEED};

use super::super::enhance::ExternalTrack;
use super::super::model::{
    AspectMode, Command, PlayerActionError, SessionEvent, SessionUpdate, TrackKind,
};
use super::super::mpv_path;
use super::{EnhancementView, LoadedTransport, MediaSourceState, MpvOp, PlayerMachine};

pub(in crate::player) type ReduceOutcome = (Vec<MpvOp>, Vec<Arc<dyn ExternalTrack>>);

impl PlayerMachine {
    /// Validates a resolved local subtitle path as UTF-8 and either emits immediate
    /// `sub-add` ops or queues it in `Loaded::deferred_local_subs` while async track
    /// additions remain in flight.
    pub fn queue_or_emit_local_subtitle(
        &mut self,
        resolved: &Path,
        has_pending_plugin_adds: bool,
        has_any_plugin_tracks: bool,
    ) -> Result<Vec<MpvOp>> {
        let utf8 = mpv_path(resolved)?.to_owned();
        let MediaSourceState::Loaded {
            identity,
            transport,
            deferred_local_subs,
            dirty,
            ..
        } = &mut self.source
        else {
            anyhow::bail!("subtitle attachment requires loaded video or audio media");
        };
        ensure!(
            self.state.loaded && matches!(identity.kind, Kind::Video | Kind::Audio),
            "subtitle attachment requires loaded video or audio media"
        );
        let in_flight_scoped = matches!(
            transport,
            LoadedTransport::PluginScoped { in_flight_adds, .. } if !in_flight_adds.is_empty()
        );
        if in_flight_scoped || has_pending_plugin_adds {
            deferred_local_subs.push(utf8);
            return Ok(Vec::new());
        }
        let restore_policy =
            matches!(transport, LoadedTransport::PluginScoped { .. }) || has_any_plugin_tracks;
        dirty.tracks = true;
        Ok(local_subtitle_ops(&utf8, restore_policy))
    }

    /// Drains `deferred_local_subs` once all scoped and plugin track adds have settled.
    pub fn drain_deferred_local_subs(
        &mut self,
        has_pending_plugin_adds: bool,
        has_any_plugin_tracks: bool,
    ) -> Vec<MpvOp> {
        if !self.state.loaded || has_pending_plugin_adds {
            return Vec::new();
        }
        let MediaSourceState::Loaded {
            transport,
            deferred_local_subs,
            dirty,
            ..
        } = &mut self.source
        else {
            return Vec::new();
        };
        if matches!(
            transport,
            LoadedTransport::PluginScoped { in_flight_adds, .. } if !in_flight_adds.is_empty()
        ) || deferred_local_subs.is_empty()
        {
            return Vec::new();
        }
        let restore_policy =
            matches!(transport, LoadedTransport::PluginScoped { .. }) || has_any_plugin_tracks;
        let mut ops = Vec::new();
        for sub in std::mem::take(deferred_local_subs) {
            ops.extend(local_subtitle_ops(&sub, restore_policy));
        }
        dirty.tracks = true;
        ops
    }

    /// Pure reducer `(state, SessionUpdate) -> Option<Vec<MpvOp>>`.
    /// Returns `None` when `update.playlist_entry_id` belongs to a superseded load.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reduce_update(
        &mut self,
        update: &SessionUpdate,
        enh: EnhancementView,
    ) -> Option<ReduceOutcome> {
        if let Some(entry_id) = update.playlist_entry_id
            && self.source.entry_id() != Some(entry_id)
        {
            return None;
        }
        Some(self.reduce_event(&update.event, enh))
    }

    pub fn reduce_event(&mut self, event: &SessionEvent, enh: EnhancementView) -> ReduceOutcome {
        let mut ops = Vec::new();
        let mut reattach = Vec::new();
        match event {
            SessionEvent::Seek | SessionEvent::QueueOverflow => {}
            SessionEvent::CommandReply { id, .. } => {
                let scoped_done = match &mut self.source {
                    MediaSourceState::Loaded {
                        transport: LoadedTransport::PluginScoped { in_flight_adds, .. },
                        ..
                    } => in_flight_adds.remove(id),
                    _ => false,
                };
                if scoped_done || enh.plugin_reply_done {
                    if let MediaSourceState::Loaded { dirty, .. } = &mut self.source {
                        dirty.tracks = true;
                    }
                    ops.extend(self.drain_deferred_local_subs(
                        enh.has_pending_plugin_adds,
                        enh.has_any_plugin_tracks,
                    ));
                }
            }
            SessionEvent::Position(v) => {
                self.state.time_pos = *v;
            }
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => {
                self.state.paused = *v;
                if !*v {
                    self.source.set_eof_paused(false);
                }
            }
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Muted(v) => self.state.muted = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => self.state.hwdec.clone_from(v),
            SessionEvent::Subtitles(v) => self.state.subs_visible = *v,
            SessionEvent::SubtitleTrack(v) => {
                if self.state.tracks.is_empty() {
                    if let MediaSourceState::Loaded { dirty, .. } = &mut self.source {
                        dirty.tracks = true;
                    }
                } else {
                    let selected_id = v.parse::<i64>().ok();
                    for t in &mut self.state.tracks {
                        if t.kind == TrackKind::Sub {
                            t.selected = selected_id == Some(t.id);
                        }
                    }
                }
            }
            SessionEvent::SubDelay(v) => self.state.sub_delay = *v,
            SessionEvent::Ended(v) => {
                self.state.ended = *v;
                if *v {
                    self.source.set_eof_paused(true);
                }
            }
            SessionEvent::Audio(v) => {
                self.state.audio.clone_from(v);
                if self.state.tracks.is_empty() {
                    if let MediaSourceState::Loaded { dirty, .. } = &mut self.source {
                        dirty.tracks = true;
                    }
                } else {
                    let selected_id = v.parse::<i64>().ok();
                    for t in &mut self.state.tracks {
                        if t.kind == TrackKind::Audio {
                            t.selected = selected_id == Some(t.id);
                        }
                    }
                }
            }
            SessionEvent::Rotation(v) => self.state.rotation = *v,
            SessionEvent::SourceRotation(_) => {}
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::AbStart(v) => self.state.ab_start = *v,
            SessionEvent::AbEnd(v) => self.state.ab_end = *v,
            SessionEvent::Chapter(v) => {
                if !enh.has_timeline_owner {
                    self.state.current_chapter = *v;
                }
            }
            SessionEvent::ChaptersChanged => {
                if let MediaSourceState::Loaded { dirty, .. } = &mut self.source {
                    dirty.chapters = true;
                }
            }
            SessionEvent::TracksChanged => {
                if let MediaSourceState::Loaded { dirty, .. } = &mut self.source {
                    dirty.tracks = true;
                }
            }
            SessionEvent::Buffering(v) => self.state.buffering = *v,
            SessionEvent::CacheDuration(v) => {
                self.state.cache_duration = if v.is_finite() { v.max(0.) } else { 0. };
            }
            SessionEvent::Loaded => {
                let (loaded_ops, tracks) = self.promote_opening_to_loaded();
                ops.extend(loaded_ops);
                reattach = tracks;
            }
            SessionEvent::Reconfigured => ops.push(MpvOp::SyncVideoParams),
            SessionEvent::Error(code) => ops.push(MpvOp::LogError(*code)),
        }
        (ops, reattach)
    }

    /// Reduces pure playback commands (`TogglePlayback`, `SetAbLoop`, `CycleAbLoop`,
    /// `ClearAbLoop`, `Seek*`, `SetVolume`, `SetSpeed`, `SetAspect`, `SetVideoEq`, etc.)
    /// into state updates and [`MpvOp`]s without touching `libmpv`.
    pub fn reduce_pure_command(&mut self, command: &Command) -> Option<Result<Vec<MpvOp>>> {
        match command {
            Command::SetAbLoop { start, end } => {
                if !(self.state.loaded
                    && !self.state.is_live
                    && matches!(self.kind(), Some(Kind::Video | Kind::Audio))
                    && start.is_finite()
                    && end.is_finite()
                    && *start >= 0.
                    && start < end
                    && (self.state.duration <= 0. || *end <= self.state.duration))
                {
                    return Some(Err(PlayerActionError::InvalidAbRange.into()));
                }
                Some(Ok(vec![
                    MpvOp::cmd_async(&["set", "ab-loop-b", "no"]),
                    MpvOp::SetPropertyF64Async {
                        name: "ab-loop-a".into(),
                        value: *start,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "ab-loop-b".into(),
                        value: *end,
                    },
                    MpvOp::cmd_async(&["set", "ab-loop-count", "inf"]),
                ]))
            }
            Command::CycleAbLoop => {
                if !(self.state.loaded
                    && !self.state.is_live
                    && matches!(self.kind(), Some(Kind::Video | Kind::Audio)))
                {
                    return Some(Err(PlayerActionError::NotSeekable.into()));
                }
                Some(Ok(vec![MpvOp::cmd_async(&["ab-loop"])]))
            }
            Command::ClearAbLoop => {
                self.state.ab_start = None;
                self.state.ab_end = None;
                Some(Ok(vec![
                    MpvOp::cmd_async(&["set", "ab-loop-b", "no"]),
                    MpvOp::cmd_async(&["set", "ab-loop-a", "no"]),
                ]))
            }
            Command::TogglePlayback => {
                if self.state.ended {
                    Some(Ok(vec![
                        MpvOp::cmd_async(&["seek", "0", "absolute+exact"]),
                        MpvOp::cmd_async(&["set", "pause", "no"]),
                    ]))
                } else {
                    Some(Ok(vec![MpvOp::cmd_async(&["cycle", "pause"])]))
                }
            }
            Command::SeekRelative(s) => Some(Ok(vec![MpvOp::cmd_async(&[
                "seek",
                &s.to_string(),
                "relative+exact",
            ])])),
            Command::SeekAbsolute(s) => Some(Ok(vec![MpvOp::cmd_async(&[
                "seek",
                &s.max(0.).to_string(),
                "absolute+exact",
            ])])),
            Command::SeekPercent(p) => Some(Ok(vec![MpvOp::cmd_async(&[
                "seek",
                &p.clamp(0., 100.).to_string(),
                "absolute-percent+exact",
            ])])),
            Command::SeekPercentFast(p) => Some(Ok(vec![MpvOp::cmd_async(&[
                "seek",
                &p.clamp(0., 100.).to_string(),
                "absolute-percent+keyframes",
            ])])),
            Command::FrameStep => Some(Ok(vec![MpvOp::cmd_async(&["frame-step"])])),
            Command::FrameBackStep => Some(Ok(vec![MpvOp::cmd_async(&["frame-back-step"])])),
            Command::ToggleMute => Some(Ok(vec![MpvOp::cmd_async(&["cycle", "mute"])])),
            Command::SetVolume(volume) => {
                let clamped = volume.clamp(0., 100.);
                let mut ops = Vec::new();
                if clamped > 0. && self.state.muted {
                    ops.push(MpvOp::cmd_async(&["set", "mute", "no"]));
                }
                ops.push(MpvOp::SetPropertyF64Async {
                    name: "volume".into(),
                    value: clamped,
                });
                Some(Ok(ops))
            }
            Command::SetSpeed(s) => Some(Ok(vec![MpvOp::SetPropertyF64Async {
                name: "speed".into(),
                value: s.clamp(MIN_SPEED, MAX_SPEED),
            }])),
            Command::ToggleSubtitles => {
                Some(Ok(vec![MpvOp::cmd_async(&["cycle", "sub-visibility"])]))
            }
            Command::NextSubtitle => {
                let mut ops = Vec::new();
                if !self.state.subs_visible {
                    ops.push(MpvOp::cmd_async(&["set", "sub-visibility", "yes"]));
                }
                ops.push(MpvOp::cmd_async(&["cycle", "sub"]));
                Some(Ok(ops))
            }
            Command::SelectSubtitle(Some(id)) => Some(Ok(vec![
                MpvOp::cmd_async(&["set", "sid", &id.to_string()]),
                MpvOp::cmd_async(&["set", "sub-visibility", "yes"]),
            ])),
            Command::SelectSubtitle(None) => {
                Some(Ok(vec![MpvOp::cmd_async(&["set", "sub-visibility", "no"])]))
            }
            Command::SubSeekPrev => Some(Ok(vec![MpvOp::cmd_async(&["sub-seek", "-1"])])),
            Command::SubSeekNext => Some(Ok(vec![MpvOp::cmd_async(&["sub-seek", "1"])])),
            Command::SubDelayBy(delta) => {
                let next = ((self.state.sub_delay + delta) * 100.).round() / 100.;
                Some(Ok(vec![MpvOp::SetPropertyF64Async {
                    name: "sub-delay".into(),
                    value: next.clamp(-60., 60.),
                }]))
            }
            Command::SetSubDelay(d) => Some(Ok(vec![MpvOp::SetPropertyF64Async {
                name: "sub-delay".into(),
                value: d.clamp(-60., 60.),
            }])),
            Command::NextAudio => Some(Ok(vec![MpvOp::cmd_async(&["cycle", "audio"])])),
            Command::SelectAudio(Some(id)) => {
                Some(Ok(vec![MpvOp::cmd_async(&["set", "aid", &id.to_string()])]))
            }
            Command::SelectAudio(None) => Some(Ok(vec![MpvOp::cmd_async(&["set", "aid", "no"])])),
            Command::SetAudioDevice(dev) => {
                let clean = if dev.trim().is_empty() || dev.chars().any(char::is_control) {
                    "auto".into()
                } else {
                    dev.trim().to_string()
                };
                self.state.audio_device.clone_from(&clean);
                Some(Ok(vec![MpvOp::cmd_async(&["set", "audio-device", &clean])]))
            }
            Command::SetLoop(enable) => Some(Ok(vec![MpvOp::cmd_async(&[
                "set",
                "loop-file",
                if *enable && !self.state.is_live {
                    "inf"
                } else {
                    "no"
                },
            ])])),
            Command::SetAspect(mode) => {
                self.state.aspect = *mode;
                let (aspect_str, panscan) = match mode {
                    AspectMode::Original => ("no", 0.),
                    AspectMode::Ratio16x9 => ("16:9", 0.),
                    AspectMode::Ratio4x3 => ("4:3", 0.),
                    AspectMode::Ratio21x9 => ("21:9", 0.),
                    AspectMode::FillCrop => ("no", 1.),
                };
                Some(Ok(vec![
                    MpvOp::cmd_async(&["set", "video-aspect-override", aspect_str]),
                    MpvOp::SetPropertyF64Async {
                        name: "panscan".into(),
                        value: panscan,
                    },
                ]))
            }
            Command::SetVideoEq {
                brightness,
                contrast,
                saturation,
                gamma,
            } => {
                let (b, c, s, g) = (
                    (*brightness).clamp(-100, 100),
                    (*contrast).clamp(-100, 100),
                    (*saturation).clamp(-100, 100),
                    (*gamma).clamp(-100, 100),
                );
                self.state.eq_brightness = b;
                self.state.eq_contrast = c;
                self.state.eq_saturation = s;
                self.state.eq_gamma = g;
                Some(Ok(vec![
                    MpvOp::SetPropertyF64Async {
                        name: "brightness".into(),
                        value: b as f64,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "contrast".into(),
                        value: c as f64,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "saturation".into(),
                        value: s as f64,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "gamma".into(),
                        value: g as f64,
                    },
                ]))
            }
            Command::ResetVideoEq => {
                self.state.eq_brightness = 0;
                self.state.eq_contrast = 0;
                self.state.eq_saturation = 0;
                self.state.eq_gamma = 0;
                Some(Ok(vec![
                    MpvOp::SetPropertyF64Async {
                        name: "brightness".into(),
                        value: 0.,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "contrast".into(),
                        value: 0.,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "saturation".into(),
                        value: 0.,
                    },
                    MpvOp::SetPropertyF64Async {
                        name: "gamma".into(),
                        value: 0.,
                    },
                ]))
            }
            Command::NextChapter => Some(Ok(vec![MpvOp::cmd_async(&["add", "chapter", "1"])])),
            Command::PrevChapter => Some(Ok(vec![MpvOp::cmd_async(&["add", "chapter", "-1"])])),
            Command::SelectChapter(ix) => Some(Ok(vec![MpvOp::cmd_async(&[
                "set",
                "chapter",
                &ix.to_string(),
            ])])),
            Command::Screenshot { path, with_subs } => {
                let target = match mpv_path(path) {
                    Ok(t) => t,
                    Err(e) => return Some(Err(e)),
                };
                let flags = if *with_subs { "subtitles" } else { "video" };
                Some(Ok(vec![MpvOp::cmd_async(&[
                    "screenshot-to-file",
                    target,
                    flags,
                ])]))
            }
            Command::VolumeBy(_)
            | Command::LoadSubtitle(_)
            | Command::RotateMedia(_)
            | Command::ToggleHFlip
            | Command::ToggleVFlip => None,
        }
    }

    /// Consumes `dirty` flags in [`MediaSourceState::Loaded`] and emits refresh ops.
    pub fn take_deferred_refresh_ops(&mut self) -> Vec<MpvOp> {
        let MediaSourceState::Loaded { dirty, .. } = &mut self.source else {
            return Vec::new();
        };
        let mut ops = Vec::new();
        if dirty.tracks {
            dirty.tracks = false;
            ops.push(MpvOp::RefreshTracks);
        }
        if dirty.chapters {
            dirty.chapters = false;
            ops.push(MpvOp::RefreshChapters);
        }
        ops
    }
}

fn local_subtitle_ops(utf8_path: &str, restore_attachment_policy: bool) -> Vec<MpvOp> {
    let mut ops = vec![
        MpvOp::DisableHlsPolicy,
        MpvOp::cmd_sync(&["sub-add", utf8_path, "select"]),
    ];
    if restore_attachment_policy {
        ops.push(MpvOp::RestoreAttachmentPolicy);
    }
    ops.push(MpvOp::cmd_async(&["set", "sub-visibility", "yes"]));
    ops
}

fn property_coalesce_slot(event: &SessionEvent) -> Option<u8> {
    match event {
        SessionEvent::Position(_) => Some(1),
        SessionEvent::Duration(_) => Some(2),
        SessionEvent::Volume(_) => Some(3),
        SessionEvent::Muted(_) => Some(4),
        SessionEvent::Speed(_) => Some(5),
        SessionEvent::Width(_) => Some(6),
        SessionEvent::Height(_) => Some(7),
        SessionEvent::Subtitles(_) => Some(8),
        SessionEvent::SubDelay(_) => Some(9),
        SessionEvent::Rotation(_) => Some(10),
        SessionEvent::SourceRotation(_) => Some(11),
        SessionEvent::LoopFile(_) => Some(12),
        SessionEvent::AbStart(_) => Some(13),
        SessionEvent::AbEnd(_) => Some(14),
        SessionEvent::Chapter(_) => Some(15),
        SessionEvent::Buffering(_) => Some(16),
        SessionEvent::CacheDuration(_) => Some(17),
        SessionEvent::TracksChanged => Some(18),
        SessionEvent::ChaptersChanged => Some(19),
        // Lifecycle barriers (`Loaded`, `Ended`, `Paused`, `Seek`, `CommandReply`,
        // `Hardware`, `Audio`, `SubtitleTrack`, `Reconfigured`, `QueueOverflow`, `Error`)
        // preserve exact ordering and are never coalesced across.
        _ => None,
    }
}

/// Coalesces redundant property updates within a drained batch while preserving
/// exact ordering around lifecycle barrier events (`Loaded`, `Ended`, `Seek`,
/// `CommandReply`, `Paused`, `Error`, `QueueOverflow`).
pub fn coalesce_updates(batch: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
    if batch.len() <= 1 {
        return batch;
    }
    let mut out: Vec<SessionUpdate> = Vec::with_capacity(batch.len());
    let mut segment_start = 0usize;
    for item in batch {
        if let Some(slot) = property_coalesce_slot(&item.event) {
            if let Some(pos) = out[segment_start..].iter().position(|existing| {
                existing.playlist_entry_id == item.playlist_entry_id
                    && property_coalesce_slot(&existing.event) == Some(slot)
            }) {
                out.remove(segment_start + pos);
            }
            out.push(item);
        } else {
            out.push(item);
            segment_start = out.len();
        }
    }
    out
}
