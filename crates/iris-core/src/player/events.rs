//! Stateful application of backend [`SessionEvent`] updates via [`PlayerMachine`]
//! and overflow resync.

use serde_json::json;

use crate::logging;

use super::machine::{EnhancementView, LoadedTransport, MediaSourceState, MpvOp};
use super::model::SessionEvent;
use super::{Session, backend, scoped};

impl Session {
    pub fn apply(&mut self, event: &SessionEvent) {
        let prev_hwdec = self.machine.state.hwdec.clone();
        let was_loaded = self.machine.state.loaded;

        let mut plugin_reply_done = false;
        if let SessionEvent::CommandReply { id, error } = event {
            self.enhancements
                .retired_luts
                .retain(|(rid, _)| *rid != *id);
            plugin_reply_done = self.enhancements.tracks.reply(
                &self.mpv,
                self.machine.source.entry_id(),
                self.machine.state.loaded,
                *id,
                *error,
            );
        }

        if let SessionEvent::SourceRotation(v) = event {
            self.visual.source_rotation = v.rem_euclid(360) as u16;
        }
        if matches!(event, SessionEvent::Loaded) {
            self.enhancements.tracks.loaded();
            self.enhancements.loading_luts.clear();
        }
        if matches!(event, SessionEvent::QueueOverflow) {
            self.resync_after_overflow();
            return;
        }

        let enh_view = EnhancementView {
            has_pending_plugin_adds: self.enhancements.tracks.has_pending_adds(),
            plugin_reply_done,
            has_any_plugin_tracks: self.enhancements.tracks.has_any_tracks(),
            has_timeline_owner: self.enhancements.timeline_owner.is_some(),
        };
        let (ops, reattach) = self.machine.reduce_event(event, enh_view);

        if let SessionEvent::Hardware(v) = event
            && prev_hwdec != *v
            && was_loaded
        {
            logging::event(
                logging::EventLevel::Info,
                "player",
                "hwdec_changed",
                json!({
                    "from": prev_hwdec,
                    "to": v,
                }),
            );
        }
        if matches!(event, SessionEvent::Position(_)) {
            self.update_plugin_chapter();
        }

        let _ = self.run_ops(ops);
        for track in reattach {
            if track.is_live()
                && let Err(err) = self.attach_plugin_track(track)
            {
                log::warn!("reattaching plugin track after GPU reset failed: {err:#}");
            }
        }
    }

    pub(in crate::player) fn run_ops(&mut self, ops: Vec<MpvOp>) -> anyhow::Result<()> {
        let mut first_err: Option<anyhow::Error> = None;
        for op in ops {
            let res: anyhow::Result<()> = match op {
                MpvOp::CommandAsync(args) => {
                    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    self.mpv.command_async(&refs).map_err(Into::into)
                }
                MpvOp::CommandWithReply { receipt, args } => {
                    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    match self.mpv.command_with_reply(receipt, &refs) {
                        Ok(()) => Ok(()),
                        Err(err) => {
                            if let MediaSourceState::Loaded {
                                transport: LoadedTransport::PluginScoped { in_flight_adds, .. },
                                ..
                            } = &mut self.machine.source
                            {
                                in_flight_adds.remove(&receipt);
                            }
                            log::warn!("could not attach scoped media track: {err}");
                            Ok(())
                        }
                    }
                }
                MpvOp::CommandSync(args) => {
                    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    self.mpv.command(&refs).map_err(Into::into)
                }
                MpvOp::SetPropertyString { name, value } => self
                    .mpv
                    .set_property_string(&name, &value)
                    .map_err(Into::into),
                MpvOp::SetPropertyF64Async { name, value } => self
                    .mpv
                    .set_property_f64_async(&name, value)
                    .map_err(Into::into),
                MpvOp::RestoreAttachmentPolicy => {
                    let r = scoped::restore_attachment_policy(&self.mpv);
                    if r.is_err() {
                        log::error!("could not restore scoped attachment policy");
                    }
                    r
                }
                MpvOp::DisableHlsPolicy => {
                    let _ = backend::disable_hls_policy(&self.mpv);
                    Ok(())
                }
                MpvOp::SyncVideoParams => {
                    self.sync_video_params();
                    Ok(())
                }
                MpvOp::QueryDurationIfMissing => {
                    if self.machine.state.duration <= 0.
                        && let Ok(v) = self.mpv.get_property_f64("duration")
                        && v.is_finite()
                        && v > 0.
                    {
                        self.machine.state.duration = v;
                    }
                    Ok(())
                }
                MpvOp::RefreshTracks => {
                    self.refresh_tracks();
                    Ok(())
                }
                MpvOp::RefreshChapters => {
                    self.refresh_chapters();
                    Ok(())
                }
                MpvOp::LogLoaded => {
                    let video_codec = self
                        .mpv
                        .get_property_string("video-codec")
                        .unwrap_or_default();
                    let audio_codec = self
                        .mpv
                        .get_property_string("audio-codec-name")
                        .unwrap_or_default();
                    logging::event(
                        logging::EventLevel::Info,
                        "player",
                        "media_loaded",
                        json!({
                            "kind": self.kind().map(|k| format!("{k:?}")),
                            "width": self.machine.state.video_size.0,
                            "height": self.machine.state.video_size.1,
                            "duration_s": self.machine.state.duration,
                            "hwdec": self.machine.state.hwdec,
                            "video_codec": video_codec,
                            "audio_codec": audio_codec,
                            "is_live": self.machine.state.is_live,
                        }),
                    );
                    Ok(())
                }
                MpvOp::LogError(code) => {
                    logging::event(
                        logging::EventLevel::Error,
                        "player",
                        "media_error",
                        json!({
                            "code": code,
                            "kind": self.kind().map(|k| format!("{k:?}")),
                            "time_pos_s": self.machine.state.time_pos,
                            "duration_s": self.machine.state.duration,
                            "hwdec": self.machine.state.hwdec,
                            "is_live": self.machine.state.is_live,
                        }),
                    );
                    self.fail_current_playback();
                    Ok(())
                }
            };
            if let Err(err) = res
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn fail_current_playback(&mut self) {
        let snapshot = self.snapshot_failed_enhancements();
        self.retire_active_lut();
        self.enhancements.loading_luts.clear();
        self.enhancements.tracks.on_end_file();
        self.machine.transition_to_failed(snapshot);
        match &self.machine.source {
            MediaSourceState::Failed {
                transport: super::machine::FailedTransport::StoppedRemote { .. },
                ..
            } => {
                self.event_pump.set_relay(Default::default());
            }
            MediaSourceState::Idle { .. } => {
                self.event_pump.set_relay(Default::default());
                self.event_pump.set_scoped(scoped::empty_scoped());
            }
            _ => {}
        }
    }

    fn sync_video_params(&mut self) {
        let i64_prop = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
        };
        if let Some(w) = i64_prop("video-out-params/dw")
            && w > 0
        {
            self.machine.state.video_size.0 = w;
        }
        if let Some(h) = i64_prop("video-out-params/dh")
            && h > 0
        {
            self.machine.state.video_size.1 = h;
        }
        if let Some(r) = i64_prop("video-out-params/rotate") {
            self.machine.state.rotation = r;
        }
        if let Some(r) = i64_prop("video-dec-params/rotate") {
            self.visual.source_rotation = r.rem_euclid(360) as u16;
        }
        if let Ok(hw) = self.mpv.get_property_string("hwdec-current") {
            self.machine.state.hwdec = hw;
        }
    }

    fn resync_after_overflow(&mut self) {
        let active_id = backend::active_playlist_entry_id(&self.mpv);
        let expected_id = self.machine.source.entry_id();
        if expected_id.is_some() && active_id != expected_id {
            if self
                .mpv
                .get_property_string("idle-active")
                .is_ok_and(|s| s == "yes")
            {
                self.fail_current_playback();
            }
            return;
        }
        if let MediaSourceState::Loaded {
            transport: LoadedTransport::PluginScoped { in_flight_adds, .. },
            ..
        } = &mut self.machine.source
        {
            in_flight_adds.clear();
        }
        self.sync_video_params();
        let f64_prop = |name: &str| {
            self.mpv
                .get_property_f64(name)
                .ok()
                .filter(|v| v.is_finite())
        };
        let flag_prop = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .map(|s| s == "yes" || s == "true")
        };
        if let Some(v) = f64_prop("time-pos") {
            self.machine.state.time_pos = v;
        }
        if let Some(v) = f64_prop("duration") {
            self.machine.state.duration = v;
        }
        if let Some(v) = flag_prop("pause") {
            self.machine.state.paused = v;
            if !v {
                self.machine.source.set_eof_paused(false);
            }
        }
        if let Some(v) = flag_prop("eof-reached") {
            self.machine.state.ended = v;
            if v {
                self.machine.source.set_eof_paused(true);
            }
        }
        if let Some(v) = f64_prop("volume") {
            self.machine.state.volume = v;
        }
        if let Some(v) = flag_prop("mute") {
            self.machine.state.muted = v;
        }
        if let Some(v) = f64_prop("speed") {
            self.machine.state.speed = v;
        }
        if let Some(v) = flag_prop("paused-for-cache") {
            self.machine.state.buffering = v;
        }
        if let Some(v) = f64_prop("demuxer-cache-duration") {
            self.machine.state.cache_duration = v.max(0.);
        }
        self.machine.state.ab_start = f64_prop("ab-loop-a").filter(|v| *v >= 0.);
        self.machine.state.ab_end = f64_prop("ab-loop-b").filter(|v| *v >= 0.);
        if !self.machine.state.loaded
            && active_id.is_some()
            && (self.machine.state.duration > 0.
                || self.machine.state.video_size.0 > 0
                || flag_prop("seeking") == Some(false))
        {
            self.enhancements.tracks.loaded();
            self.enhancements.loading_luts.clear();
            let (ops, reattach) = self.machine.promote_opening_to_loaded();
            let _ = self.run_ops(ops);
            for track in reattach {
                if track.is_live()
                    && let Err(err) = self.attach_plugin_track(track)
                {
                    log::warn!("reattaching plugin track after overflow failed: {err:#}");
                }
            }
        }
        let sub_ops = self.machine.drain_deferred_local_subs(
            self.enhancements.tracks.has_pending_adds(),
            self.enhancements.tracks.has_any_tracks(),
        );
        let _ = self.run_ops(sub_ops);
        if let MediaSourceState::Loaded { dirty, .. } = &mut self.machine.source {
            dirty.tracks = true;
            dirty.chapters = true;
        }
    }
}
