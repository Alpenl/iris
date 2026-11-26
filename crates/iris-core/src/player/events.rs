//! Stateful application of backend [`SessionEvent`] updates and overflow resync.

use serde_json::json;

use crate::logging;

use super::Session;
use super::backend;
use super::model::{SessionEvent, TrackKind};

impl Session {
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Seek => {}
            SessionEvent::CommandReply { id, error } => {
                self.enhancements
                    .retired_luts
                    .retain(|(rid, _)| *rid != *id);
                let scoped_done = self.in_flight_scoped_adds.remove(id);
                let plugin_done = self.enhancements.tracks.reply(
                    &self.mpv,
                    self.playlist_entry_id,
                    self.state.loaded,
                    *id,
                    *error,
                );
                if scoped_done || plugin_done {
                    self.tracks_dirty = true;
                }
                if (scoped_done || plugin_done) && !self.deferred_local_subs.is_empty() {
                    self.flush_deferred_local_subs();
                }
            }
            SessionEvent::Position(v) => {
                self.state.time_pos = *v;
                self.update_plugin_chapter();
            }
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => {
                self.state.paused = *v;
                if !*v {
                    self.eof_paused = false;
                }
            }
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Muted(v) => self.state.muted = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => {
                if self.state.hwdec != *v && self.state.loaded {
                    logging::event(
                        logging::EventLevel::Info,
                        "player",
                        "hwdec_changed",
                        json!({
                            "from": self.state.hwdec,
                            "to": v,
                        }),
                    );
                }
                self.state.hwdec.clone_from(v);
            }
            SessionEvent::Subtitles(v) => self.state.subs_visible = *v,
            SessionEvent::SubtitleTrack(v) => {
                if self.state.tracks.is_empty() {
                    self.tracks_dirty = true;
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
                    self.eof_paused = true;
                }
            }
            SessionEvent::Audio(v) => {
                self.state.audio.clone_from(v);
                if self.state.tracks.is_empty() {
                    self.tracks_dirty = true;
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
            SessionEvent::SourceRotation(v) => {
                self.visual.source_rotation = v.rem_euclid(360) as u16;
            }
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::AbStart(v) => self.state.ab_start = *v,
            SessionEvent::AbEnd(v) => self.state.ab_end = *v,
            SessionEvent::Chapter(v) => {
                if self.enhancements.timeline_owner.is_none() {
                    self.state.current_chapter = *v;
                }
            }
            SessionEvent::ChaptersChanged => self.chapters_dirty = true,
            SessionEvent::TracksChanged => self.tracks_dirty = true,
            SessionEvent::Buffering(v) => self.state.buffering = *v,
            SessionEvent::CacheDuration(v) => {
                self.state.cache_duration = if v.is_finite() { v.max(0.) } else { 0. };
            }
            SessionEvent::Loaded => {
                self.enhancements.tracks.loaded();
                self.enhancements.loading_luts.clear();
                self.state.loaded = true;
                self.sync_video_params();
                if self.state.duration <= 0.
                    && let Ok(v) = self.mpv.get_property_f64("duration")
                    && v.is_finite()
                    && v > 0.
                {
                    self.state.duration = v;
                }
                self.attach_scoped_tracks();
                self.attach_pending_remote_subs();
                self.reattach_plugin_tracks();
                self.tracks_dirty = true;
                self.chapters_dirty = true;
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
                        "kind": self.kind.map(|k| format!("{k:?}")),
                        "width": self.state.video_size.0,
                        "height": self.state.video_size.1,
                        "duration_s": self.state.duration,
                        "hwdec": self.state.hwdec,
                        "video_codec": video_codec,
                        "audio_codec": audio_codec,
                        "is_live": self.state.is_live,
                    }),
                );
            }
            SessionEvent::Reconfigured => self.sync_video_params(),
            SessionEvent::QueueOverflow => self.resync_after_overflow(),
            SessionEvent::Error(code) => {
                logging::event(
                    logging::EventLevel::Error,
                    "player",
                    "media_error",
                    json!({
                        "code": code,
                        "kind": self.kind.map(|k| format!("{k:?}")),
                        "time_pos_s": self.state.time_pos,
                        "duration_s": self.state.duration,
                        "hwdec": self.state.hwdec,
                        "is_live": self.state.is_live,
                    }),
                );
                self.preserve_source_on_error();
                self.enhancements.error_lut = self
                    .enhancements
                    .monitor_lut
                    .as_ref()
                    .map(std::sync::Arc::downgrade);
                self.enhancements.error_tracks = self.enhancements.tracks.snapshot_wanted_weak();
                if let Some(owner) = self.enhancements.timeline_owner.take() {
                    let chapters = std::mem::take(&mut self.state.chapters);
                    self.enhancements.error_timeline = Some((owner, chapters));
                }
                self.retire_active_lut();
                self.enhancements.loading_luts.clear();
                self.enhancements.tracks.on_end_file();
                self.enhancements.reattach_on_load.clear();
                self.in_flight_scoped_adds.clear();
                self.deferred_local_subs.clear();
                self.state.loaded = false;
            }
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
            self.state.video_size.0 = w;
        }
        if let Some(h) = i64_prop("video-out-params/dh")
            && h > 0
        {
            self.state.video_size.1 = h;
        }
        if let Some(r) = i64_prop("video-out-params/rotate") {
            self.state.rotation = r;
        }
        if let Some(r) = i64_prop("video-dec-params/rotate") {
            self.visual.source_rotation = r.rem_euclid(360) as u16;
        }
        if let Ok(hw) = self.mpv.get_property_string("hwdec-current") {
            self.state.hwdec = hw;
        }
    }

    fn resync_after_overflow(&mut self) {
        let active_id = backend::active_playlist_entry_id(&self.mpv);
        if self.playlist_entry_id.is_some() && active_id != self.playlist_entry_id {
            if self
                .mpv
                .get_property_string("idle-active")
                .is_ok_and(|s| s == "yes")
            {
                self.enhancements.error_lut = self
                    .enhancements
                    .monitor_lut
                    .as_ref()
                    .map(std::sync::Arc::downgrade);
                self.enhancements.error_tracks = self.enhancements.tracks.snapshot_wanted_weak();
                if let Some(owner) = self.enhancements.timeline_owner.take() {
                    let chapters = std::mem::take(&mut self.state.chapters);
                    self.enhancements.error_timeline = Some((owner, chapters));
                }
                self.retire_active_lut();
                self.enhancements.loading_luts.clear();
                self.enhancements.tracks.on_end_file();
                self.enhancements.reattach_on_load.clear();
                self.in_flight_scoped_adds.clear();
                self.deferred_local_subs.clear();
                self.state.loaded = false;
            }
            return;
        }
        self.in_flight_scoped_adds.clear();
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
            self.state.time_pos = v;
        }
        if let Some(v) = f64_prop("duration") {
            self.state.duration = v;
        }
        if let Some(v) = flag_prop("pause") {
            self.state.paused = v;
            if !v {
                self.eof_paused = false;
            }
        }
        if let Some(v) = flag_prop("eof-reached") {
            self.state.ended = v;
            if v {
                self.eof_paused = true;
            }
        }
        if let Some(v) = f64_prop("volume") {
            self.state.volume = v;
        }
        if let Some(v) = flag_prop("mute") {
            self.state.muted = v;
        }
        if let Some(v) = f64_prop("speed") {
            self.state.speed = v;
        }
        if let Some(v) = flag_prop("paused-for-cache") {
            self.state.buffering = v;
        }
        if let Some(v) = f64_prop("demuxer-cache-duration") {
            self.state.cache_duration = v.max(0.);
        }
        self.state.ab_start = f64_prop("ab-loop-a").filter(|v| *v >= 0.);
        self.state.ab_end = f64_prop("ab-loop-b").filter(|v| *v >= 0.);
        if !self.state.loaded
            && active_id.is_some()
            && (self.state.duration > 0.
                || self.state.video_size.0 > 0
                || flag_prop("seeking") == Some(false))
        {
            self.enhancements.tracks.loaded();
            self.enhancements.loading_luts.clear();
            self.state.loaded = true;
            self.attach_scoped_tracks();
            self.attach_pending_remote_subs();
            self.reattach_plugin_tracks();
        }
        if !self.deferred_local_subs.is_empty() {
            self.flush_deferred_local_subs();
        }
        self.tracks_dirty = true;
        self.chapters_dirty = true;
    }
}
