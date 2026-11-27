//! Hardened `libmpv` instance initialization, property observation, event pump,
//! and backend command/filter execution on [`Session`].

use std::sync::Arc;

use anyhow::Result;
use iris_mpv::{Format, Mpv};

use crate::library::Kind;
use crate::settings::{Hwdec, OnEof, Settings, SubLang};

use super::model::{AspectMode, Command, MAX_SPEED, MIN_SPEED, PlayerActionError};
use super::{MediaSourceState, MonitorLutFilter, Session};
mod commands;
mod pump;
mod references;
pub(super) use pump::{EventPump, active_playlist_entry_id, spawn_event_pump};
use pump::{HOOK_LOAD, HOOK_PRELOADED, prop};

pub(super) fn create() -> Result<Arc<Mpv>> {
    create_with_options(false, false)
}

pub(super) fn create_headless(paused: bool) -> Result<Arc<Mpv>> {
    create_with_options(true, paused)
}

fn create_with_options(headless: bool, paused: bool) -> Result<Arc<Mpv>> {
    let mut mpv = Mpv::create()?;
    for (k, v) in [
        ("vo", if headless { "null" } else { "libmpv" }),
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
    if headless {
        mpv.set_option("ao", "null")?;
        if paused {
            mpv.set_option("pause", "yes")?;
        }
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

    pub(in crate::player) fn log_mpv_err<T>(
        context: &str,
        result: std::result::Result<T, iris_mpv::Error>,
    ) {
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
            self.mpv
                .command_async(&["set", "slang", sub_lang.mpv_slang()]),
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

    pub(super) fn retire_active_lut(&mut self) {
        let Some(old) = self.enhancements.monitor_lut.take() else {
            return;
        };
        if matches!(self.kind, Some(Kind::Video | Kind::Photo)) {
            Self::log_mpv_err(
                "reset hwdec",
                self.mpv
                    .command_async(&["set", "hwdec", self.effective_hwdec_str()]),
            );
        }
        match self.receipts.next().and_then(|receipt| {
            self.mpv
                .command_with_reply(receipt, &["set", "vf", ""])
                .map(|()| receipt)
                .map_err(anyhow::Error::from)
        }) {
            Ok(receipt) => self.enhancements.retired_luts.push((receipt, old)),
            Err(err) => {
                log::debug!("mpv tracked lut clear fallback: {err}");
                Self::log_mpv_err("reset vf", self.mpv.command_async(&["set", "vf", ""]));
                self.enhancements.loading_luts.push(old);
            }
        }
    }

    pub(super) fn reset_backend_for_load(&mut self) -> Result<()> {
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
        if self.enhancements.monitor_lut.is_some() {
            self.retire_active_lut();
        } else {
            Self::log_mpv_err("reset vf", self.mpv.command_async(&["set", "vf", ""]));
        }
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
                    self.enhancements.retired_luts.push((receipt, old));
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
        if track_reply && let Ok(receipt) = self.receipts.next() {
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
                    Self::log_mpv_err(
                        "select first video track",
                        self.mpv.command_async(&["set", "vid", "1"]),
                    );
                }
                return;
            }
        }
        let monitor_lut = self
            .enhancements
            .monitor_lut
            .clone()
            .or_else(|| self.enhancements.error_lut.take().and_then(|w| w.upgrade()));
        let saved_timeline = self
            .enhancements
            .timeline_owner
            .clone()
            .map(|owner| (owner, self.state.chapters.clone()))
            .or_else(|| self.enhancements.error_timeline.take());
        let mut saved_plugin_tracks = self.enhancements.tracks.snapshot_wanted_live();
        if saved_plugin_tracks.is_empty() {
            saved_plugin_tracks = std::mem::take(&mut self.enhancements.error_tracks)
                .into_iter()
                .filter_map(|w| w.upgrade())
                .filter(|t| t.is_live())
                .collect();
        }
        let private_identity = self.private_identity;
        let (ab_start, ab_end) = (self.state.ab_start, self.state.ab_end);
        let (hflip, vflip, extra_rotate, aspect, sub_delay, paused, eof_paused, pos) = (
            self.state.hflip,
            self.state.vflip,
            self.visual.extra_rotate,
            self.state.aspect,
            self.state.sub_delay,
            self.state.paused,
            self.eof_paused,
            self.state.time_pos,
        );
        let reloaded = if let Some(path) = self.current.clone() {
            let (kind, photo) = (self.kind, self.photo_meta.clone());
            self.load(&path, kind, photo, pos).is_ok()
        } else if let MediaSourceState::DirectRemote { last_remote, .. }
        | MediaSourceState::StoppedRemote { last_remote } = &self.source
        {
            let req = (**last_remote).clone();
            self.load_remote(req, settings, pos).is_ok()
        } else if let MediaSourceState::PluginScoped { playback, .. } = &self.source {
            let playback = Arc::clone(playback);
            if playback.stream.is_cancelled() {
                self.discard_scoped();
                self.state.loaded = false;
                false
            } else {
                self.current_media_uri().is_some_and(|uri| {
                    self.reopen_scoped_after_gpu_reset(playback, uri, settings, pos)
                        .is_ok()
                })
            }
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
        self.enhancements.reattach_on_load = saved_plugin_tracks;
        if let Some((owner, chapters)) = saved_timeline {
            self.enhancements.timeline_owner = Some(owner);
            self.state.chapters = chapters;
            self.update_plugin_chapter();
        }
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
                Self::log_mpv_err(
                    "restore ab-loop-a",
                    self.mpv.set_property_f64_async("ab-loop-a", start),
                );
                Self::log_mpv_err(
                    "restore ab-loop-b",
                    self.mpv.set_property_f64_async("ab-loop-b", end),
                );
                Self::log_mpv_err(
                    "restore ab-loop-count",
                    self.mpv.command_async(&["set", "ab-loop-count", "inf"]),
                );
                self.state.ab_start = Some(start);
                self.state.ab_end = Some(end);
            }
            self.state.paused = paused;
            self.eof_paused = eof_paused;
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use iris_mpv::Mpv;

    use crate::library::Kind;
    use crate::player::{Command, MonitorLutFilter, Session};

    struct StubMonitorLut(PathBuf);

    impl MonitorLutFilter for StubMonitorLut {
        fn filter_spec(&self) -> anyhow::Result<String> {
            let path = self.0.to_str().expect("utf-8 test path");
            #[cfg(windows)]
            let owned = path.replace('\\', "/");
            #[cfg(windows)]
            let path = owned.as_str();
            let escape = |value: &str, special: &str| {
                let mut out = String::new();
                for ch in value.chars() {
                    if special.contains(ch) || ch.is_ascii_whitespace() {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out
            };
            let option = escape(path, ":\\'");
            let graph = format!(
                "lut3d=file={}:interp=trilinear",
                escape(&option, "[]=;,\\'")
            );
            Ok(format!(
                "@iris-monitor-lut:lavfi=graph=%{}%{}",
                graph.len(),
                graph
            ))
        }
    }

    #[test]
    fn monitor_lut_loads_through_native_filter_and_releases_on_stop() {
        let dir = crate::test_util::TestDir::new("lut-[,;' space");
        let cube_path = dir.join("preview.cube");
        std::fs::write(
            &cube_path,
            format!("LUT_3D_SIZE 2\n{}", "1 0 0\n".repeat(8)),
        )
        .unwrap();
        let lut: Arc<dyn MonitorLutFilter> = Arc::new(StubMonitorLut(cube_path));
        let mut session = Session::new_headless(true).unwrap();
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
        session.set_monitor_lut(Some(Arc::clone(&lut))).unwrap();
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
        session.stop().unwrap();
        assert_eq!(session.effective_hwdec_str(), "auto-safe");
        session.shutdown();
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
