//! Hardened `libmpv` instance initialization, property observation, event pump,
//! and backend command/filter execution on [`Session`].

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv, Value};
use serde_json::json;

use crate::library::Kind;
use crate::logging::{self, EventLevel};
use crate::settings::{Hwdec, OnEof, Settings, SubLang};

use super::{MediaSourceState, MonitorLutFilter, ScopedStream, Session, next_command_receipt};
use super::model::{
    AspectMode, Command, MAX_SPEED, MIN_SPEED, PlayerActionError, SessionEvent, SessionUpdate,
};
use super::relay::Service;
mod commands;
mod references;
use references::{disable_hls, enable_hls};

const HOOK_LOAD: u64 = 1;
const HOOK_PRELOADED: u64 = 2;

pub(super) mod prop {
    pub const TIME_POS: u64 = 1;
    pub const DURATION: u64 = 2;
    pub const PAUSE: u64 = 3;
    pub const VOLUME: u64 = 4;
    pub const SPEED: u64 = 5;
    pub const VIDEO_W: u64 = 6;
    pub const VIDEO_H: u64 = 7;
    pub const HWDEC: u64 = 8;
    pub const SUB_VISIBLE: u64 = 9;
    pub const EOF: u64 = 10;
    pub const AUDIO: u64 = 11;
    pub const ROTATION: u64 = 12;
    pub const MUTE: u64 = 13;
    pub const SUB: u64 = 14;
    pub const TRACK_COUNT: u64 = 15;
    pub const LOOP_FILE: u64 = 16;
    pub const SUB_DELAY: u64 = 17;
    pub const CHAPTER: u64 = 18;
    pub const CHAPTER_COUNT: u64 = 19;
    pub const PAUSED_FOR_CACHE: u64 = 20;
    pub const CACHE_DURATION: u64 = 21;
    pub const SOURCE_ROTATION: u64 = 22;
    pub const AB_START: u64 = 23;
    pub const AB_END: u64 = 24;
}

pub(super) fn create() -> Result<Arc<Mpv>> {
    let mut mpv = Mpv::create()?;
    for (k, v) in [
        ("vo", "libmpv"),
        ("hwdec", "auto-safe"),
        ("keep-open", "yes"),
        ("idle", "yes"),
        ("image-display-duration", "inf"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("config", "no"),
        ("volume-max", "100"),
        ("terminal", "no"),
        ("access-references", "no"),
        ("sub-auto", "fuzzy"),
        (
            "slang",
            "zh-Hans,zh-CN,chs,sc,zh,chi,zho,zh-Hant,zh-TW,cht,tc,en,eng",
        ),
        ("subs-fallback", "yes"),
    ] {
        mpv.set_option(k, v)?;
    }
    for option in [
        "load-scripts",
        "osc",
        "ytdl",
        "load-stats-overlay",
        "load-console",
        "load-select",
        "load-positioning",
        "load-context-menu",
        "load-commands",
        "load-auto-profiles",
    ] {
        match mpv.set_option(option, "no") {
            Ok(()) => {}
            Err(e) if e.code == -5 => {}
            Err(e) => return Err(e.into()),
        }
    }
    mpv.initialize()?;
    mpv.request_log_messages("warn")?;
    for (id, name, format) in [
        (prop::TIME_POS, "time-pos", Format::Double),
        (prop::DURATION, "duration", Format::Double),
        (prop::PAUSE, "pause", Format::Flag),
        (prop::VOLUME, "volume", Format::Double),
        (prop::SPEED, "speed", Format::Double),
        (prop::VIDEO_W, "video-out-params/dw", Format::Int),
        (prop::VIDEO_H, "video-out-params/dh", Format::Int),
        (prop::HWDEC, "hwdec-current", Format::Str),
        (prop::SUB_VISIBLE, "sub-visibility", Format::Flag),
        (prop::EOF, "eof-reached", Format::Flag),
        (prop::AUDIO, "aid", Format::Str),
        (prop::ROTATION, "video-out-params/rotate", Format::Int),
        (prop::MUTE, "mute", Format::Flag),
        (prop::SUB, "sid", Format::Str),
        (prop::TRACK_COUNT, "track-list/count", Format::Int),
        (prop::LOOP_FILE, "loop-file", Format::Str),
        (prop::AB_START, "ab-loop-a", Format::Str),
        (prop::AB_END, "ab-loop-b", Format::Str),
        (prop::SUB_DELAY, "sub-delay", Format::Double),
        (prop::CHAPTER, "chapter", Format::Int),
        (prop::CHAPTER_COUNT, "chapter-list/count", Format::Int),
        (prop::PAUSED_FOR_CACHE, "paused-for-cache", Format::Flag),
        (
            prop::CACHE_DURATION,
            "demuxer-cache-duration",
            Format::Double,
        ),
        (
            prop::SOURCE_ROTATION,
            "video-dec-params/rotate",
            Format::Int,
        ),
    ] {
        mpv.observe_property(id, name, format)?;
    }
    mpv.add_hook(HOOK_LOAD, "on_load")?;
    mpv.add_hook(HOOK_PRELOADED, "on_preloaded")?;
    Ok(Arc::new(mpv))
}

pub(super) struct EventPump {
    done: Arc<AtomicBool>,
    relay: Arc<Mutex<Weak<Service>>>,
    scoped: Arc<Mutex<Weak<dyn ScopedStream>>>,
    thread: Cell<Option<JoinHandle<()>>>,
}

impl EventPump {
    pub(super) fn set_scoped(
        &self,
        scoped: Weak<dyn ScopedStream>,
    ) -> Weak<dyn ScopedStream> {
        std::mem::replace(&mut *self.scoped.lock().unwrap(), scoped)
    }
    pub(super) fn set_relay(&self, relay: Weak<Service>) -> Weak<Service> {
        std::mem::replace(&mut *self.relay.lock().unwrap(), relay)
    }

    pub(super) fn stop(&self, mpv: &Mpv) {
        if let Some(thread) = self.thread.take() {
            // Continue any in-flight load hook until mpv acknowledges quit.
            if mpv.command_async(&["quit"]).is_err() {
                self.done.store(true, Ordering::Release);
                mpv.abort_video_recovery();
                mpv.wakeup();
            }
            let _ = thread.join();
        }
    }
}

pub(super) fn active_playlist_entry_id(mpv: &Mpv) -> Option<i64> {
    let pos: i64 = mpv
        .get_property_string("playlist-playing-pos")
        .ok()?
        .parse()
        .ok()
        .filter(|&p| p >= 0)?;
    mpv.get_property_string(&format!("playlist/{pos}/id"))
        .ok()?
        .parse()
        .ok()
}

pub(super) fn spawn_event_pump(
    mpv: Arc<Mpv>,
) -> Result<(EventPump, UnboundedReceiver<SessionUpdate>)> {
    let (tx, rx) = unbounded();
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let relay = Arc::new(Mutex::new(Weak::<Service>::new()));
    let source = relay.clone();
    let scoped = Arc::new(Mutex::new(super::scoped::empty_scoped()));
    let scoped_source = scoped.clone();
    let thread = thread::Builder::new()
        .name("iris-mpv-events".into())
        .spawn(move || {
            let mut playing = None;
            let mut hls = false;
            let mut restricted = false;
            let mut last_decode_burst: Option<Instant> = None;
            let mut suppressed_decode_errors: u32 = 0;
            while !stopped.load(Ordering::Acquire) {
                let Some(raw) = mpv.wait_event(-1.0) else {
                    continue;
                };
                let scope = match &raw {
                    Event::Shutdown => break,
                    Event::CommandReply { .. } => None,
                    Event::StartFile(id) => {
                        hls = false;
                        restricted = false;
                        playing = Some(*id);
                        continue;
                    }
                    Event::EndFile {
                        playlist_entry_id, ..
                    } => {
                        if playing == Some(*playlist_entry_id) {
                            playing = None;
                        }
                        Some(*playlist_entry_id)
                    }
                    Event::Hook { userdata, id } => {
                        match *userdata {
                            HOOK_LOAD => {
                                restricted = scoped_source.lock().unwrap().upgrade().is_some_and(
                                    |session| {
                                        session.root_url().ok()
                                            == mpv.get_property_string("stream-open-filename").ok()
                                    },
                                );
                                let service = source.lock().unwrap().upgrade();
                                if let Some(service) = service {
                                    match enable_hls(&mpv, &service) {
                                        Ok(prepared) => hls = prepared,
                                        Err(_) => {
                                            let _ = disable_hls(&mpv);
                                            log::warn!("could not prepare scoped HLS playback");
                                        }
                                    }
                                }
                            }
                            HOOK_PRELOADED if restricted => {
                                if super::scoped::restore_attachment_policy(&mpv).is_err() {
                                    log::error!("could not restore scoped attachment policy");
                                    let _ = mpv.command_async(&["stop"]);
                                }
                                restricted = false;
                            }
                            HOOK_PRELOADED if hls => {
                                // The HLS demuxer captures its open policy. Future
                                // attachments must see the restrictive defaults.
                                if disable_hls(&mpv).is_err() {
                                    log::error!("could not restore mpv reference policy");
                                    let _ = mpv.command_async(&["stop"]);
                                }
                                hls = false;
                            }
                            _ => (),
                        }
                        // SAFETY: This event came from this handle's wait_event call, and this
                        // is its only continuation after completing the hook's work.
                        let _ = unsafe { mpv.continue_hook(*id) };
                        continue;
                    }
                    Event::QueueOverflow => {
                        playing = active_playlist_entry_id(&mpv);
                        mpv.reseed_video_recovery(playing);
                        log::warn!("mpv event queue overflow; resyncing active entry {playing:?}");
                        None
                    }
                    Event::Log {
                        prefix,
                        level,
                        text,
                    } => {
                        let detail = logging::redact_text(text.trim_end());
                        let mapped = match level.as_str() {
                            "fatal" | "error" => log::Level::Error,
                            "warn" => log::Level::Warn,
                            "info" => log::Level::Info,
                            _ => log::Level::Debug,
                        };
                        log::log!(target: "iris::mpv", mapped, "{prefix}/{level}: {detail}");
                        if prefix == "vd" && detail.contains("Attempting next decoding method") {
                            logging::event(
                                EventLevel::Warn,
                                "player",
                                "decode_fallback",
                                json!({ "prefix": prefix, "detail": detail }),
                            );
                        } else if matches!(prefix.as_str(), "vd" | "vo" | "d3d11")
                            || (prefix == "ffmpeg" && level == "error")
                        {
                            let now = Instant::now();
                            let due = last_decode_burst
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5));
                            if due {
                                let skipped = std::mem::take(&mut suppressed_decode_errors);
                                last_decode_burst = Some(now);
                                logging::event(
                                    EventLevel::Warn,
                                    "player",
                                    "mpv_diagnostic",
                                    json!({
                                        "prefix": prefix,
                                        "mpv_level": level,
                                        "detail": detail,
                                        "suppressed_since_last": skipped,
                                    }),
                                );
                            } else {
                                suppressed_decode_errors =
                                    suppressed_decode_errors.saturating_add(1);
                            }
                        }
                        continue;
                    }
                    Event::PropertyChange { id, .. }
                        if matches!(
                            *id,
                            prop::VOLUME | prop::MUTE | prop::SPEED | prop::SUB_VISIBLE
                        ) =>
                    {
                        None
                    }
                    _ => match playing {
                        Some(id) => Some(id),
                        None => continue,
                    },
                };
                if let Some(event) = SessionEvent::from_backend(raw) {
                    let _ = tx.unbounded_send(SessionUpdate {
                        playlist_entry_id: scope,
                        event,
                    });
                }
            }
        })?;
    Ok((
        EventPump {
            done,
            relay,
            scoped,
            thread: Cell::new(Some(thread)),
        },
        rx,
    ))
}

impl SessionEvent {
    pub(super) fn from_backend(event: Event) -> Option<Self> {
        Some(match event {
            Event::Seek => Self::Seek,
            Event::CommandReply { id, error } => Self::CommandReply { id, error },
            Event::PropertyChange { id, value, .. } => match (id, value) {
                (prop::TIME_POS, Value::Double(v)) => Self::Position(v),
                (prop::TIME_POS, Value::None) => Self::Position(0.),
                (prop::DURATION, Value::Double(v)) => Self::Duration(v),
                (prop::PAUSE, Value::Flag(v)) => Self::Paused(v),
                (prop::VOLUME, Value::Double(v)) => Self::Volume(v),
                (prop::MUTE, Value::Flag(v)) => Self::Muted(v),
                (prop::SPEED, Value::Double(v)) => Self::Speed(v),
                (prop::VIDEO_W, Value::Int(v)) => Self::Width(v),
                (prop::VIDEO_H, Value::Int(v)) => Self::Height(v),
                (prop::HWDEC, Value::Str(v)) => Self::Hardware(v),
                (prop::HWDEC, Value::None) => Self::Hardware(String::new()),
                (prop::SUB_VISIBLE, Value::Flag(v)) => Self::Subtitles(v),
                (prop::SUB, Value::Str(v)) => Self::SubtitleTrack(v),
                (prop::SUB, Value::None) => Self::SubtitleTrack(String::new()),
                (prop::SUB_DELAY, Value::Double(v)) => Self::SubDelay(v),
                (prop::EOF, Value::Flag(v)) => Self::Ended(v),
                (prop::AUDIO, Value::Str(v)) => Self::Audio(v),
                (prop::AUDIO, Value::None) => Self::Audio(String::new()),
                (prop::ROTATION, Value::Int(v)) => Self::Rotation(v),
                (prop::SOURCE_ROTATION, Value::Int(v)) => Self::SourceRotation(v),
                (prop::TRACK_COUNT, Value::Int(_)) => Self::TracksChanged,
                (prop::LOOP_FILE, Value::Str(v)) => Self::LoopFile(v == "inf" || v == "yes"),
                (prop::AB_START, Value::Str(v)) => {
                    Self::AbStart(v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.))
                }
                (prop::AB_END, Value::Str(v)) => {
                    Self::AbEnd(v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.))
                }
                (prop::CHAPTER, Value::Int(v)) => Self::Chapter(v),
                (prop::CHAPTER, Value::None) => Self::Chapter(-1),
                (prop::CHAPTER_COUNT, Value::Int(_)) => Self::ChaptersChanged,
                (prop::PAUSED_FOR_CACHE, Value::Flag(v)) => Self::Buffering(v),
                (prop::PAUSED_FOR_CACHE, Value::None) => Self::Buffering(false),
                (prop::CACHE_DURATION, Value::Double(v)) => Self::CacheDuration(v),
                (prop::CACHE_DURATION, Value::None) => Self::CacheDuration(0.),
                _ => return None,
            },
            Event::FileLoaded => Self::Loaded,
            Event::VideoReconfig => Self::Reconfigured,
            Event::QueueOverflow => Self::QueueOverflow,
            Event::EndFile {
                reason: iris_mpv::EndReason::Error,
                error,
                ..
            } => Self::Error(error),
            _ => return None,
        })
    }
}

impl Session {
    fn effective_hwdec_str(&self) -> &'static str {
        match self.user_hwdec.get() {
            Hwdec::Off => "no",
            Hwdec::AutoSafe => {
                if (self.kind == Some(Kind::Video) && (self.state.hflip || self.state.vflip))
                    || (matches!(self.kind, Some(Kind::Video | Kind::Photo))
                        && self.enhancements.monitor_lut.is_some())
                {
                    "auto-copy"
                } else {
                    "auto-safe"
                }
            }
        }
    }

    fn log_mpv_err<T>(context: &str, result: std::result::Result<T, iris_mpv::Error>) {
        if let Err(error) = result {
            log::debug!("mpv {context} ignored: {error}");
        }
    }

    pub fn set_hwdec(&self, hwdec: Hwdec) {
        self.user_hwdec.set(hwdec);
        Self::log_mpv_err(
            "set hwdec",
            self.mpv
                .command_async(&["set", "hwdec", self.effective_hwdec_str()]),
        );
    }

    pub fn set_sub_lang(&self, sub_lang: SubLang) {
        Self::log_mpv_err(
            "set slang",
            self.mpv.command_async(&["set", "slang", sub_lang.mpv_slang()]),
        );
    }

    pub fn set_sub_scale(&self, scale: f64) {
        Self::log_mpv_err(
            "set sub-scale",
            self.mpv
                .set_property_f64_async("sub-scale", scale.clamp(0.5, 2.5)),
        );
    }

    pub(super) fn loop_value(&self, live: bool) -> &'static str {
        if !live && self.on_eof.get() == OnEof::Loop {
            "inf"
        } else {
            "no"
        }
    }

    pub fn set_on_eof_loop(&self, on_eof: OnEof) {
        self.on_eof.set(on_eof);
        Self::log_mpv_err(
            "set loop-file",
            self.mpv
                .command_async(&["set", "loop-file", self.loop_value(self.state.is_live)]),
        );
    }

    pub fn apply_settings(&self, settings: &Settings) {
        self.set_hwdec(settings.hwdec);
        self.set_sub_lang(settings.sub_lang);
        self.set_sub_scale(settings.sub_scale);
        self.set_on_eof_loop(settings.on_eof);
        let dev = settings.audio_device.trim();
        if !dev.is_empty() && !dev.chars().any(char::is_control) {
            Self::log_mpv_err(
                "set audio-device",
                self.mpv.command_async(&["set", "audio-device", dev]),
            );
        }
    }

    pub(super) fn reset_backend_for_load(&self) -> Result<()> {
        self.mpv.command_async(&["set", "ab-loop-b", "no"])?;
        self.mpv.command_async(&["set", "ab-loop-a", "no"])?;
        self.transform(0., [0., 0.])?;
        Self::log_mpv_err(
            "reset video-rotate",
            self.mpv.command_async(&["set", "video-rotate", "0"]),
        );
        Self::log_mpv_err(
            "reset video-aspect-override",
            self.mpv
                .command_async(&["set", "video-aspect-override", "no"]),
        );
        Self::log_mpv_err(
            "reset panscan",
            self.mpv.set_property_f64_async("panscan", 0.),
        );
        Self::log_mpv_err(
            "reset sub-delay",
            self.mpv.set_property_f64_async("sub-delay", 0.),
        );
        Self::log_mpv_err(
            "reset hwdec",
            self.mpv
                .command_async(&["set", "hwdec", self.user_hwdec.get().mpv_value()]),
        );
        Self::log_mpv_err("reset vf", self.mpv.command_async(&["set", "vf", ""]));
        Ok(())
    }

    pub fn transform(&self, zoom: f64, pan: [f64; 2]) -> Result<()> {
        self.mpv.set_property_f64_async("video-zoom", zoom)?;
        self.mpv.set_property_f64_async("video-pan-x", pan[0])?;
        self.mpv.set_property_f64_async("video-pan-y", pan[1])?;
        Ok(())
    }

    pub fn subtitle_inset(&mut self, logical_inset: f64, viewport_height: f64) {
        let offset = if viewport_height > 0. {
            logical_inset * 720. / viewport_height
        } else {
            0.
        };
        if (offset - self.visual.subtitle_inset).abs() < 0.1 {
            return;
        }
        self.visual.subtitle_inset = offset;
        if let Err(e) =
            self.mpv
                .command_async(&["set", "sub-margin-y-offset", &offset.round().to_string()])
        {
            log::error!("subtitle safe area: {e}");
        }
    }

    fn uses_filter_orientation(&self) -> bool {
        self.photo_meta
            .as_ref()
            .is_some_and(crate::photo::Photo::uses_filter_orientation)
    }

    /// Clockwise rotation of source axes into the display axes.
    pub fn display_rotation(&self) -> u16 {
        let base = if let Some(photo) = &self.photo_meta {
            let r = photo.display_rotation();
            if r != 0
                || photo.exif_orientation.is_some()
                || photo.mirror.is_some()
                || photo.size.is_some()
            {
                r
            } else {
                self.visual.source_rotation
            }
        } else {
            self.visual.source_rotation
        };
        (base + self.visual.extra_rotate) % 360
    }

    pub fn display_size(&self) -> (i64, i64) {
        let (w, h) = self.state.video_size;
        // The HEIF format filter writes rotation metadata, leaving the frame axes unchanged.
        if self.display_axes_swapped() {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub(super) fn display_axes_swapped(&self) -> bool {
        self.display_rotation() % 180 == 90
    }

    /// Returns user mirror flags in horizontal/vertical display order.
    pub fn display_flips(&self) -> (bool, bool) {
        let (hflip, vflip) = (self.state.hflip, self.state.vflip);
        if self.display_axes_swapped() {
            (vflip, hflip)
        } else {
            (hflip, vflip)
        }
    }

    /// Accepts only a background-validated data artifact, never a guest filter.
    /// This is a CPU preview transform; color-managed output is a separate stage.
    pub fn set_monitor_lut(&mut self, lut: Option<Arc<dyn MonitorLutFilter>>) -> Result<()> {
        if lut.is_some()
            && (!self.state.loaded || !matches!(self.kind, Some(Kind::Photo | Kind::Video)))
        {
            return Err(PlayerActionError::NoVisualMedia.into());
        }
        let previous = std::mem::replace(&mut self.enhancements.monitor_lut, lut);
        match self.apply_vf_filters_tracked(previous.is_some()) {
            Ok(Some(receipt)) => {
                if let Some(old) = previous {
                    self.enhancements.retired_lut = Some((receipt, old));
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error) => {
                self.enhancements.monitor_lut = previous;
                let _ = self.apply_vf_filters();
                Err(error)
            }
        }
    }

    fn apply_vf_filters(&self) -> Result<()> {
        self.apply_vf_filters_tracked(false).map(|_| ())
    }

    fn apply_vf_filters_tracked(&self, track_reply: bool) -> Result<Option<u64>> {
        if matches!(self.kind, Some(Kind::Video | Kind::Photo)) {
            let _ = self
                .mpv
                .command_async(&["set", "hwdec", self.effective_hwdec_str()]);
        }
        let mut vf = if self.kind == Some(Kind::Photo) {
            let filter_rot = self.uses_filter_orientation();
            let fallback = crate::photo::Photo::default();
            let meta = self.photo_meta.as_ref().unwrap_or(&fallback);
            meta.vf_chain_with_transforms(
                self.visual.extra_rotate,
                self.state.hflip,
                self.state.vflip,
                filter_rot,
            )
            .unwrap_or_default()
        } else {
            let mut chain = Vec::new();
            if self.state.hflip {
                chain.push("hflip");
            }
            if self.state.vflip {
                chain.push("vflip");
            }
            chain.join(",")
        };
        if let Some(lut) = &self.enhancements.monitor_lut {
            if !vf.is_empty() {
                vf.push(',');
            }
            vf.push_str(&lut.filter_spec()?);
        }
        if track_reply && let Ok(receipt) = next_command_receipt() {
            self.mpv.command_with_reply(receipt, &["set", "vf", &vf])?;
            return Ok(Some(receipt));
        }
        self.mpv.command_async(&["set", "vf", &vf])?;
        Ok(None)
    }

    pub fn recover_unloaded_after_gpu_reset(
        &mut self,
        settings: &Settings,
        zoom: f64,
        pan: [f64; 2],
    ) {
        if self.kind != Some(Kind::Photo) {
            let has_path = self
                .mpv
                .get_property_string("path")
                .is_ok_and(|p| !p.is_empty());
            if has_path {
                if self.kind == Some(Kind::Video)
                    && self
                        .mpv
                        .get_property_string("current-vo")
                        .map(|vo| vo.is_empty())
                        .unwrap_or(true)
                    && self
                        .mpv
                        .get_property_string("vid")
                        .map(|vid| vid.is_empty() || vid == "no")
                        .unwrap_or(true)
                {
                    let _ = self.mpv.command_async(&["set", "vid", "1"]);
                }
                return;
            }
        }
        let monitor_lut = self.enhancements.monitor_lut.clone();
        let private_identity = self.private_identity;
        let (ab_start, ab_end) = (self.state.ab_start, self.state.ab_end);
        let (hflip, vflip, extra_rotate, aspect, sub_delay, paused, pos) = (
            self.state.hflip,
            self.state.vflip,
            self.visual.extra_rotate,
            self.state.aspect,
            self.state.sub_delay,
            self.state.paused,
            self.state.time_pos,
        );
        let reloaded = if let Some(path) = self.current.clone() {
            let (kind, photo) = (self.kind, self.photo_meta.clone());
            self.load(&path, kind, photo, pos).is_ok()
        } else if let MediaSourceState::DirectRemote { last_remote, .. } = &self.source {
            let req = last_remote.clone();
            self.load_remote(req, settings, pos).is_ok()
        } else if let MediaSourceState::PluginScoped { playback, .. } = &self.source {
            let playback = Arc::clone(playback);
            self.current_media_uri()
                .is_some_and(|uri| self.open_scoped(playback, uri, settings, pos).is_ok())
        } else {
            false
        };
        if !reloaded {
            return;
        }
        self.private_identity = private_identity;
        self.state.hflip = hflip;
        self.state.vflip = vflip;
        self.visual.extra_rotate = extra_rotate;
        self.enhancements.monitor_lut = monitor_lut;
        if hflip
            || vflip
            || self.enhancements.monitor_lut.is_some()
            || (self.uses_filter_orientation() && extra_rotate != 0)
        {
            let _ = self.apply_vf_filters();
        }
        if !self.uses_filter_orientation() && extra_rotate != 0 {
            let _ = self
                .mpv
                .command_async(&["set", "video-rotate", &extra_rotate.to_string()]);
        }
        if aspect != AspectMode::Original {
            let _ = self.execute(Command::SetAspect(aspect));
        }
        if sub_delay.abs() > 0.001 {
            let _ = self.execute(Command::SetSubDelay(sub_delay));
        }
        if matches!(self.kind, Some(Kind::Video | Kind::Audio)) {
            if let (Some(start), Some(end)) = (ab_start, ab_end)
                && !self.state.is_live
                && start < end
            {
                let _ = self.mpv.set_property_f64_async("ab-loop-a", start);
                let _ = self.mpv.set_property_f64_async("ab-loop-b", end);
                let _ = self.mpv.command_async(&["set", "ab-loop-count", "inf"]);
                self.state.ab_start = Some(start);
                self.state.ab_end = Some(end);
            }
            let _ = self
                .mpv
                .command_async(&["set", "pause", if paused { "yes" } else { "no" }]);
        }
        if zoom != 0. || pan != [0., 0.] {
            let _ = self.transform(zoom, pan);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn monitor_lut_loads_through_native_filter_and_releases_on_stop() {
        use crate::plugin_v2::{
            files::TempRoot,
            ledger::{Ledger, Resource},
            lut::MonitorLut,
        };
        use crate::vfs::HttpCancel;
        let parent = crate::test_util::TestDir::new("lut-[,;' space");
        let ledger = Ledger::default();
        let root = TempRoot::new(&parent, "lut-test".into(), ledger.clone()).unwrap();
        let cancel = HttpCancel::default();
        let mut writer = root.writer("cube").unwrap();
        writer
            .write(
                format!("LUT_3D_SIZE 2\n{}", "1 0 0\n".repeat(8)).as_bytes(),
                &cancel,
            )
            .unwrap();
        let lut = MonitorLut::prepare(writer.finish(&cancel).unwrap(), &ledger, &cancel).unwrap();
        let mut session = Session::new().unwrap();
        session.mpv.set_property_string("vo", "null").unwrap();
        let video = format!(
            "memory://YUV4MPEG2 W16 H16 F10:1 Ip A1:1 C420jpeg\n{}",
            format!("FRAME\n{}", "x".repeat(384)).repeat(30)
        );
        session
            .mpv
            .loadfile_with_options(&video, "replace", &[("pause", "yes")])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.mpv.get_property_f64("time-pos").is_err() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        session.kind = Some(Kind::Video);
        session.state.loaded = true;
        session.set_monitor_lut(Some(lut.clone())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let filters = session.mpv.get_property_string("vf").unwrap();
            if filters.contains("iris-monitor-lut")
                && session
                    .mpv
                    .get_property_string("video-out-params/pixelformat")
                    .is_ok_and(|s| s.starts_with("rgb"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native LUT filter unavailable: {filters}, pixel format {:?}",
                session
                    .mpv
                    .get_property_string("video-out-params/pixelformat")
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session.effective_hwdec_str(), "auto-copy");
        drop(lut);
        session.stop().unwrap();
        assert_eq!(session.effective_hwdec_str(), "auto-safe");
        session.shutdown();
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.is_quiescent() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        drop(root);
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::TempBytes), 0);
    }

    #[test]
    fn subtitle_cue_commands_seek_between_cues_while_paused() {
        let mut session = Session::new().unwrap();
        session.mpv.set_property_string("vo", "null").unwrap();
        // Six seconds of 2x2 YUV420 video. ASCII samples keep the memory URL NUL-free.
        let video = format!(
            "memory://YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
            "FRAME\nxxxxxx".repeat(60)
        );
        let subs = concat!(
            "memory://1\n00:00:01,000 --> 00:00:02,000\nFirst\n\n",
            "2\n00:00:04,000 --> 00:00:05,000\nSecond\n"
        );
        session
            .mpv
            .loadfile_with_options(
                &video,
                "replace",
                &[
                    ("pause", "yes"),
                    ("start", "1.1"),
                    ("sub-files-append", subs),
                ],
            )
            .unwrap();

        let wait_for_cue = |mpv: &Mpv, text: &str, start: f64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let position = mpv.get_property_f64("time-pos").ok();
                let subtitle = mpv.get_property_string("sub-text").ok();
                if position.is_some_and(|pos| (pos - start).abs() <= 0.2)
                    && subtitle.as_deref() == Some(text)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected {text} at {start}s, got {subtitle:?} at {position:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for_cue(&session.mpv, "First", 1.0);
        session.execute(Command::SubSeekNext).unwrap();
        wait_for_cue(&session.mpv, "Second", 4.0);
        session.execute(Command::SubSeekPrev).unwrap();
        wait_for_cue(&session.mpv, "First", 1.0);
        assert_eq!(session.mpv.get_property_string("pause").unwrap(), "yes");
        session.shutdown();
    }
}
