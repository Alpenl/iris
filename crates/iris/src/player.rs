//! Viewport-owned playback session. All client commands and backend names live here.

use std::sync::Arc;
use std::path::{Path, PathBuf};
use crate::library::{self, Entry, Kind};
use crate::photo;

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv, Value};

mod prop {
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
}

fn create() -> Result<Arc<Mpv>> {
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
        // Pick Chinese subtitles first, then English; with no match, still show
        // a full (non-forced) track, as PotPlayer does.
        (
            "slang",
            "zh-Hans,zh-CN,chs,sc,zh,chi,zho,zh-Hant,zh-TW,cht,tc,en,eng",
        ),
        ("subs-fallback", "yes"),
    ] {
        mpv.set_option(k, v)?;
    }
    // Script options do not exist when mpv was compiled without the relevant
    // runtime (our release SDK has no Lua). Absence already guarantees no
    // script runs; every other option error remains fatal.
    for option in ["load-scripts", "osc", "ytdl", "load-stats-overlay", "load-console",
        "load-select", "load-positioning", "load-context-menu", "load-commands", "load-auto-profiles"] {
        match mpv.set_option(option, "no") {
            Ok(()) => {},
            Err(e) if e.code == -5 => {}, // MPV_ERROR_OPTION_NOT_FOUND: compiled out.
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
    ] {
        mpv.observe_property(id, name, format)?;
    }
    Ok(Arc::new(mpv))
}

/// Forwards mpv events from a blocking thread to an async channel. The thread
/// ends (and drops its handle reference) when mpv shuts down.
fn spawn_event_pump(mpv: Arc<Mpv>) -> UnboundedReceiver<SessionEvent> {
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
                        log::warn!(target: "iris::mpv", "{prefix}/{level}: {}", text.trim_end());
                    }
                    Some(ev) => {
                        if let Some(event) = SessionEvent::from_backend(ev) {
                            if tx.unbounded_send(event).is_err() { break; }
                        }
                    }
                    None => {}
                }
            }
        })
        .expect("spawn mpv event thread");
    rx
}

#[derive(Debug)]
pub enum SessionEvent {
    Position(f64), Duration(f64), Paused(bool), Volume(f64), Speed(f64),
    Width(i64), Height(i64), Hardware(String), Subtitles(bool), Ended(bool),
    Audio(String), Rotation(i64), Loaded, Reconfigured, Error(i32),
}

impl SessionEvent {
    fn from_backend(event: Event) -> Option<Self> {
        Some(match event {
            Event::PropertyChange { id, value, .. } => match (id, value) {
                (prop::TIME_POS, Value::Double(v)) => Self::Position(v),
                (prop::TIME_POS, Value::None) => Self::Position(0.),
                (prop::DURATION, Value::Double(v)) => Self::Duration(v),
                (prop::PAUSE, Value::Flag(v)) => Self::Paused(v),
                (prop::VOLUME, Value::Double(v)) => Self::Volume(v),
                (prop::SPEED, Value::Double(v)) => Self::Speed(v),
                (prop::VIDEO_W, Value::Int(v)) => Self::Width(v),
                (prop::VIDEO_H, Value::Int(v)) => Self::Height(v),
                (prop::HWDEC, Value::Str(v)) => Self::Hardware(v),
                (prop::HWDEC, Value::None) => Self::Hardware(String::new()),
                (prop::SUB_VISIBLE, Value::Flag(v)) => Self::Subtitles(v),
                (prop::EOF, Value::Flag(v)) => Self::Ended(v),
                (prop::AUDIO, Value::Str(v)) => Self::Audio(v),
                (prop::ROTATION, Value::Int(v)) => Self::Rotation(v),
                _ => return None,
            },
            Event::FileLoaded => Self::Loaded,
            Event::VideoReconfig => Self::Reconfigured,
            Event::EndFile { reason: iris_mpv::EndReason::Error, error } => Self::Error(error),
            _ => return None,
        })
    }
}

pub struct PlaybackState {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub speed: f64,
    pub video_size: (i64, i64),
    pub hwdec: String,
    pub subs_visible: bool,
    pub ended: bool,
    pub audio: String,
    pub rotation: i64,
}

pub enum Command {
    TogglePlayback, SeekRelative(f64), SeekPercent(f64), VolumeBy(f64), SetVolume(f64),
    SpeedBy(f64), ToggleSubtitles, NextSubtitle, NextAudio,
}

pub struct Session {
    mpv: Arc<Mpv>,
    pub state: PlaybackState,
    pub folder: Option<PathBuf>,
    pub entries: Vec<Entry>,
    /// The absolute filesystem path is the identity, never an index or content hash.
    pub current: Option<PathBuf>,
    kind: Option<Kind>,
    subtitle_inset: f64,
}

impl Session {
    pub fn new() -> Result<Self> {
        Ok(Self {
            mpv: create()?,
            state: PlaybackState { time_pos: 0., duration: 0., paused: false, volume: 100.,
                speed: 1., video_size: (0, 0), hwdec: String::new(), subs_visible: true,
                ended: false, audio: String::new(), rotation: 0 },
            folder: None, entries: Vec::new(), current: None, kind: None, subtitle_inset: 0.,
        })
    }

    pub fn render_handle(&self) -> Arc<Mpv> { self.mpv.clone() }
    pub fn events(&self) -> UnboundedReceiver<SessionEvent> { spawn_event_pump(self.mpv.clone()) }
    pub fn kind(&self) -> Option<Kind> { self.kind }
    pub fn index(&self) -> Option<usize> {
        self.current.as_ref().and_then(|path| self.entries.iter().position(|e| &e.path == path))
    }

    pub fn open(&mut self, path: &Path) -> Result<()> {
        let path = path.canonicalize()?;
        let folder = if path.is_dir() { path.clone() } else {
            path.parent().unwrap_or(Path::new(".")).to_path_buf()
        };
        self.entries = library::scan(&folder);
        self.folder = Some(folder);
        if path.is_file() { self.load(&path, library::kind_of(&path))?; }
        else if let Some(first) = self.entries.first() {
            let (path, kind) = (first.path.clone(), first.kind);
            self.load(&path, Some(kind))?;
        } else {
            self.mpv.command_async(&["stop"])?;
            self.current = None;
            self.kind = None;
        }
        Ok(())
    }

    pub fn play(&mut self, index: usize) -> Result<()> {
        if let Some(entry) = self.entries.get(index) {
            let (path, kind) = (entry.path.clone(), entry.kind);
            self.load(&path, Some(kind))?;
        }
        Ok(())
    }

    pub fn step(&mut self, delta: isize) -> Result<()> {
        if !self.entries.is_empty() {
            let index = self.index().map(|i| i as isize + delta).unwrap_or(0)
                .rem_euclid(self.entries.len() as isize) as usize;
            self.play(index)?;
        }
        Ok(())
    }

    fn load(&mut self, path: &Path, kind: Option<Kind>) -> Result<()> {
        // Async loadfile copies argv; a path never becomes an option or command.
        let url = path.to_string_lossy();
        let opts = (kind == Some(Kind::Photo)).then(|| photo::probe(path).mpv_options()).flatten();
        self.reset_transform()?;
        match opts {
            Some(opts) => self.mpv.command_async(&["loadfile", &url, "replace", "-1", &opts])?,
            None => self.mpv.command_async(&["loadfile", &url])?,
        }
        self.current = Some(path.to_path_buf());
        self.kind = kind.or(Some(Kind::Video));
        self.state.ended = false;
        self.state.time_pos = 0.;
        self.state.duration = 0.;
        Ok(())
    }

    pub fn execute(&self, command: Command) -> Result<()> {
        match command {
            Command::TogglePlayback => {
                if self.state.ended {
                    self.mpv.command_async(&["seek", "0", "absolute+exact"])?;
                    self.mpv.command_async(&["set", "pause", "no"])?;
                } else { self.mpv.command_async(&["cycle", "pause"])?; }
            }
            Command::SeekRelative(seconds) => self.mpv.command_async(&["seek", &seconds.to_string(), "relative+exact"])? ,
            Command::SeekPercent(percent) => self.mpv.command_async(&["seek", &percent.clamp(0.,100.).to_string(), "absolute-percent+exact"])? ,
            Command::VolumeBy(delta) => self.mpv.command_async(&["add", "volume", &delta.to_string()])?,
            Command::SetVolume(volume) => self.mpv.set_property_f64_async("volume", volume.clamp(0.,100.))?,
            Command::SpeedBy(factor) => self.mpv.command_async(&["multiply", "speed", &factor.to_string()])?,
            Command::ToggleSubtitles => self.mpv.command_async(&["cycle", "sub-visibility"])? ,
            Command::NextSubtitle => self.mpv.command_async(&["cycle", "sub"])? ,
            Command::NextAudio => self.mpv.command_async(&["cycle", "audio"])? ,
        }
        Ok(())
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Position(v) => self.state.time_pos = *v,
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => self.state.paused = *v,
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => self.state.hwdec.clone_from(v),
            SessionEvent::Subtitles(v) => self.state.subs_visible = *v,
            SessionEvent::Ended(v) => self.state.ended = *v,
            SessionEvent::Audio(v) => self.state.audio.clone_from(v),
            SessionEvent::Rotation(v) => self.state.rotation = *v,
            _ => {}
        }
    }

    pub fn transform(&self, zoom: f64, pan: [f64; 2]) -> Result<()> {
        self.mpv.set_property_f64_async("video-zoom", zoom)?;
        self.mpv.set_property_f64_async("video-pan-x", pan[0])?;
        self.mpv.set_property_f64_async("video-pan-y", pan[1])?;
        Ok(())
    }
    fn reset_transform(&self) -> Result<()> { self.transform(0., [0., 0.]) }

    pub fn subtitle_inset(&mut self, logical_inset: f64, viewport_height: f64) {
        let offset = if viewport_height > 0. { logical_inset * 720. / viewport_height } else { 0. };
        if (offset - self.subtitle_inset).abs() < 0.1 { return; }
        self.subtitle_inset = offset;
        if let Err(e) = self.mpv.command_async(&["set", "sub-margin-y-offset", &offset.round().to_string()]) {
            log::error!("subtitle safe area: {e}");
        }
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_property(&self, name: &str) -> Result<String> { Ok(self.mpv.get_property_string(name)?) }
    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_command(&self, args: &[&str]) -> Result<()> { Ok(self.mpv.command(args)?) }
    pub fn shutdown(&self) { let _ = self.mpv.command(&["quit"]); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_safe_area_changes_backend_margin_and_restores_it() {
        let mut session = Session::new().unwrap();
        session.mpv.observe_property(100, "sub-margin-y-offset", Format::Int).unwrap();
        let wait_for = |mpv: &Mpv, value| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if let Some(Event::PropertyChange { id: 100, value: Value::Int(actual), .. }) = mpv.wait_event(0.05) {
                    if actual == value { return; }
                }
            }
            panic!("subtitle safe margin never became {value}");
        };
        session.subtitle_inset(132., 900.);
        wait_for(&session.mpv, 106);
        session.subtitle_inset(0., 900.);
        wait_for(&session.mpv, 0);
        session.shutdown();
    }
}
