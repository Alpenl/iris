//! Hardened `libmpv` instance initialization, property observation, event pump,
//! and backend command/filter execution on [`Session`].

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv, Value};

use crate::library::Kind;
use crate::settings::{Hwdec, OnEof, Settings, SubLang};

use super::Session;
use super::model::{AspectMode, Command, SessionEvent};

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
}

pub(super) fn create() -> Result<Arc<Mpv>> {
    let mpv = Mpv::create()?;
    for (k, v) in [
        ("vo", "libmpv"),
        ("hwdec", "auto-safe"),
        ("keep-open", "yes"),
        ("idle", "yes"),
        ("image-display-duration", "inf"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("config", "no"),
        ("terminal", "no"),
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
        (prop::SUB_DELAY, "sub-delay", Format::Double),
        (prop::CHAPTER, "chapter", Format::Int),
        (prop::CHAPTER_COUNT, "chapter-list/count", Format::Int),
        (prop::PAUSED_FOR_CACHE, "paused-for-cache", Format::Flag),
        (
            prop::CACHE_DURATION,
            "demuxer-cache-duration",
            Format::Double,
        ),
    ] {
        mpv.observe_property(id, name, format)?;
    }
    Ok(Arc::new(mpv))
}

pub(super) fn spawn_event_pump(mpv: Arc<Mpv>) -> UnboundedReceiver<SessionEvent> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("iris-mpv-events".into())
        .spawn(move || {
            loop {
                match mpv.wait_event(-1.0) {
                    Some(Event::Shutdown) => break,
                    Some(Event::Log {
                        prefix,
                        level,
                        text,
                    }) => {
                        // Demuxers include stream URLs and relative signed paths in errors.
                        let detail = if text.contains("://") || text.contains('?') {
                            "media URL omitted"
                        } else {
                            text.trim_end()
                        };
                        log::warn!(target: "iris::mpv", "{prefix}/{level}: {detail}");
                    }
                    Some(ev) => {
                        if let Some(event) = SessionEvent::from_backend(ev)
                            && tx.unbounded_send(event).is_err()
                        {
                            break;
                        }
                    }
                    None => {}
                }
            }
        })
        .expect("spawn mpv event thread");
    rx
}

impl SessionEvent {
    pub(super) fn from_backend(event: Event) -> Option<Self> {
        Some(match event {
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
                (prop::TRACK_COUNT, Value::Int(_)) => Self::TracksChanged,
                (prop::LOOP_FILE, Value::Str(v)) => Self::LoopFile(v == "inf" || v == "yes"),
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
            Event::EndFile {
                reason: iris_mpv::EndReason::Error,
                error,
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
                if self.kind == Some(Kind::Video) && (self.state.hflip || self.state.vflip) {
                    "auto-copy"
                } else {
                    "auto-safe"
                }
            }
        }
    }

    pub fn set_hwdec(&self, hwdec: Hwdec) {
        self.user_hwdec.set(hwdec);
        let _ = self
            .mpv
            .command_async(&["set", "hwdec", self.effective_hwdec_str()]);
    }

    pub fn set_sub_lang(&self, sub_lang: SubLang) {
        let _ = self
            .mpv
            .command_async(&["set", "slang", sub_lang.mpv_slang()]);
    }

    pub fn set_sub_scale(&self, scale: f64) {
        let _ = self
            .mpv
            .set_property_f64_async("sub-scale", scale.clamp(0.5, 2.5));
    }

    pub fn set_on_eof_loop(&self, on_eof: OnEof) {
        let loop_val = if on_eof == OnEof::Loop { "inf" } else { "no" };
        let _ = self.mpv.command_async(&["set", "loop-file", loop_val]);
    }

    pub fn apply_settings(&self, settings: &Settings) {
        self.set_hwdec(settings.hwdec);
        self.set_sub_lang(settings.sub_lang);
        self.set_sub_scale(settings.sub_scale);
        self.set_on_eof_loop(settings.on_eof);
        let dev = settings.audio_device.trim();
        if !dev.is_empty() && !dev.chars().any(char::is_control) {
            let _ = self.mpv.command_async(&["set", "audio-device", dev]);
        }
    }

    pub(super) fn reset_backend_for_load(&self) -> Result<()> {
        self.transform(0., [0., 0.])?;
        let _ = self.mpv.command_async(&["set", "video-rotate", "0"]);
        let _ = self
            .mpv
            .command_async(&["set", "video-aspect-override", "no"]);
        let _ = self.mpv.set_property_f64_async("panscan", 0.);
        let _ = self.mpv.set_property_f64_async("sub-delay", 0.);
        let _ = self
            .mpv
            .command_async(&["set", "hwdec", self.user_hwdec.get().mpv_value()]);
        let _ = self.mpv.command_async(&["set", "vf", ""]);
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
        if (offset - self.subtitle_inset).abs() < 0.1 {
            return;
        }
        self.subtitle_inset = offset;
        if let Err(e) =
            self.mpv
                .command_async(&["set", "sub-margin-y-offset", &offset.round().to_string()])
        {
            log::error!("subtitle safe area: {e}");
        }
    }

    fn is_current_heif(&self) -> bool {
        if self.kind != Some(Kind::Photo) {
            return false;
        }
        if let Some(ext) = self
            .current
            .as_deref()
            .and_then(Path::extension)
            .and_then(|e| e.to_str())
        {
            return matches!(ext.to_ascii_lowercase().as_str(), "heic" | "heif");
        }
        self.current_uri
            .as_deref()
            .and_then(|u| u.split(['?', '#']).next())
            .and_then(|u| Path::new(u).extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif"))
    }

    fn apply_vf_filters(&self) -> Result<()> {
        let vf = if self.kind == Some(Kind::Photo) {
            let is_heif = self.is_current_heif();
            let fallback = crate::photo::Photo::default();
            let meta = self.photo_meta.as_ref().unwrap_or(&fallback);
            meta.vf_chain_with_transforms(
                if is_heif { self.extra_rotate } else { 0 },
                self.state.hflip,
                self.state.vflip,
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
            if self.kind == Some(Kind::Video) {
                let _ = self
                    .mpv
                    .command_async(&["set", "hwdec", self.effective_hwdec_str()]);
            }
            chain.join(",")
        };
        self.mpv.command_async(&["set", "vf", &vf])?;
        Ok(())
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
                {
                    let _ = self.mpv.command_async(&["set", "vid", "1"]);
                }
                return;
            }
        }
        let (hflip, vflip, extra_rotate, aspect, sub_delay, paused, pos) = (
            self.state.hflip,
            self.state.vflip,
            self.extra_rotate,
            self.state.aspect,
            self.state.sub_delay,
            self.state.paused,
            self.state.time_pos,
        );
        let reloaded = if let Some(path) = self.current.clone() {
            let (kind, photo) = (self.kind, self.photo_meta.clone());
            self.load(&path, kind, photo, pos).is_ok()
        } else if let Some(req) = self.last_remote.clone() {
            self.open_remote(req, settings, pos).is_ok()
        } else {
            false
        };
        if !reloaded {
            return;
        }
        self.state.hflip = hflip;
        self.state.vflip = vflip;
        self.extra_rotate = extra_rotate;
        if hflip || vflip || (self.is_current_heif() && extra_rotate != 0) {
            let _ = self.apply_vf_filters();
        }
        if !self.is_current_heif() && extra_rotate != 0 {
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
            let _ = self
                .mpv
                .command_async(&["set", "pause", if paused { "yes" } else { "no" }]);
        }
        if zoom != 0. || pan != [0., 0.] {
            let _ = self.transform(zoom, pan);
        }
    }

    pub fn execute(&mut self, command: Command) -> Result<()> {
        match command {
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
                let next = (self.state.volume + delta).clamp(0., 100.);
                if next > 0. && self.state.muted {
                    self.mpv.command_async(&["set", "mute", "no"])?;
                }
                self.mpv.set_property_f64_async("volume", next)?;
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
                .set_property_f64_async("speed", s.clamp(0.25, 4.))?,
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
                let url = path.to_string_lossy();
                self.mpv.command_async(&["sub-add", &url, "select"])?;
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
                self.mpv
                    .command_async(&["set", "loop-file", if enable { "inf" } else { "no" }])?;
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
                if self.is_current_heif() {
                    self.apply_vf_filters()?;
                } else {
                    self.mpv.command_async(&[
                        "set",
                        "video-rotate",
                        &self.extra_rotate.to_string(),
                    ])?;
                }
            }
            Command::ToggleHFlip => {
                self.state.hflip = !self.state.hflip;
                self.apply_vf_filters()?;
            }
            Command::ToggleVFlip => {
                self.state.vflip = !self.state.vflip;
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

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
                let _ = mpv.wait_event(0.05);
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
