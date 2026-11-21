//! Viewport-owned playback session. All client commands and backend names live here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv, Value};

use crate::library::{self, Entry, Kind};
use crate::photo::{self, Photo};
use crate::settings::{OnEof, Settings};

pub const SPEED_STEPS: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

pub fn step_speed(current: f64, dir: isize) -> f64 {
    if dir > 0 {
        SPEED_STEPS
            .iter()
            .copied()
            .find(|&s| s > current + 0.01)
            .unwrap_or(*SPEED_STEPS.last().unwrap())
    } else if dir < 0 {
        SPEED_STEPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < current - 0.01)
            .unwrap_or(SPEED_STEPS[0])
    } else {
        current
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Sub,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub kind: TrackKind,
    pub title: String,
    pub lang: String,
    pub codec: String,
    pub selected: bool,
    pub external: bool,
}

impl Track {
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if !self.title.is_empty() {
            parts.push(self.title.clone());
        }
        if !self.lang.is_empty()
            && !self
                .title
                .to_ascii_lowercase()
                .contains(&self.lang.to_ascii_lowercase())
        {
            parts.push(self.lang.clone());
        }
        if !self.codec.is_empty() {
            parts.push(self.codec.to_ascii_uppercase());
        }
        if parts.is_empty() {
            parts.push(format!("轨道 {}", self.id));
        }
        let mut text = parts.join(" · ");
        if self.external {
            text.push_str(" [外挂]");
        }
        text
    }
}

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
    pub const MUTE: u64 = 13;
    pub const SUB: u64 = 14;
    pub const TRACK_COUNT: u64 = 15;
    pub const LOOP_FILE: u64 = 16;
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
            Err(e) if e.code == -5 => {} // MPV_ERROR_OPTION_NOT_FOUND: compiled out.
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

#[derive(Debug)]
pub enum SessionEvent {
    Position(f64),
    Duration(f64),
    Paused(bool),
    Volume(f64),
    Muted(bool),
    Speed(f64),
    Width(i64),
    Height(i64),
    Hardware(String),
    Subtitles(bool),
    SubtitleTrack(String),
    Ended(bool),
    Audio(String),
    Rotation(i64),
    LoopFile(bool),
    TracksChanged,
    Loaded,
    Reconfigured,
    Error(i32),
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
                (prop::MUTE, Value::Flag(v)) => Self::Muted(v),
                (prop::SPEED, Value::Double(v)) => Self::Speed(v),
                (prop::VIDEO_W, Value::Int(v)) => Self::Width(v),
                (prop::VIDEO_H, Value::Int(v)) => Self::Height(v),
                (prop::HWDEC, Value::Str(v)) => Self::Hardware(v),
                (prop::HWDEC, Value::None) => Self::Hardware(String::new()),
                (prop::SUB_VISIBLE, Value::Flag(v)) => Self::Subtitles(v),
                (prop::SUB, Value::Str(v)) => Self::SubtitleTrack(v),
                (prop::SUB, Value::None) => Self::SubtitleTrack(String::new()),
                (prop::EOF, Value::Flag(v)) => Self::Ended(v),
                (prop::AUDIO, Value::Str(v)) => Self::Audio(v),
                (prop::AUDIO, Value::None) => Self::Audio(String::new()),
                (prop::ROTATION, Value::Int(v)) => Self::Rotation(v),
                (prop::TRACK_COUNT, Value::Int(_)) => Self::TracksChanged,
                (prop::LOOP_FILE, Value::Str(v)) => Self::LoopFile(v == "inf" || v == "yes"),
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

pub struct PlaybackState {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub video_size: (i64, i64),
    pub hwdec: String,
    pub subs_visible: bool,
    pub sub_id: String,
    pub ended: bool,
    pub audio: String,
    pub rotation: i64,
    pub loop_file: bool,
    pub tracks: Vec<Track>,
    pub photo_colour: Option<&'static str>,
    /// The current path has finished loading; positions now belong to it.
    pub loaded: bool,
}

pub enum Command {
    TogglePlayback,
    SeekRelative(f64),
    SeekPercent(f64),
    SeekPercentFast(f64),
    FrameStep,
    FrameBackStep,
    ToggleMute,
    VolumeBy(f64),
    SetVolume(f64),
    SpeedStep(isize),
    SetSpeed(f64),
    ToggleSubtitles,
    NextSubtitle,
    SelectSubtitle(Option<i64>),
    LoadSubtitle(PathBuf),
    NextAudio,
    SelectAudio(Option<i64>),
    ToggleLoop,
    SetLoop(bool),
    RotatePhoto(i64),
}

/// A folder scan and photo probe done off the UI thread, ready to load.
pub struct Prepared {
    folder: PathBuf,
    entries: Vec<Entry>,
    target: Option<PathBuf>,
    kind: Option<Kind>,
    photo: Option<Photo>,
    options: Option<String>,
}

impl Prepared {
    /// The file that will load, if any; records are keyed by this path.
    pub fn target(&self) -> Option<&Path> {
        self.target.as_deref()
    }
}

/// Resolves `path` to its folder listing and load options. Blocking file I/O
/// only (directory scan, photo headers); safe to run on a background thread.
/// `cached` reuses an existing listing of the same folder instead of rescanning.
pub fn prepare(path: &Path, cached: Option<(PathBuf, Vec<Entry>)>) -> Result<Prepared> {
    let path = path.canonicalize()?;
    let (folder, file) = if path.is_dir() {
        (path, None)
    } else {
        (
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Some(path),
        )
    };
    let entries = match cached {
        Some((dir, entries)) if dir == folder => entries,
        _ => library::scan(&folder),
    };
    let (target, kind) = match file {
        Some(file) => {
            let kind = library::kind_of(&file);
            (Some(file), kind)
        }
        None => entries
            .first()
            .map_or((None, None), |e| (Some(e.path.clone()), Some(e.kind))),
    };
    let photo = (kind == Some(Kind::Photo))
        .then(|| target.as_deref().map(photo::probe))
        .flatten();
    let options = photo.as_ref().and_then(Photo::mpv_options);
    Ok(Prepared {
        folder,
        entries,
        target,
        kind,
        photo,
        options,
    })
}

pub struct Session {
    mpv: Arc<Mpv>,
    pub state: PlaybackState,
    pub folder: Option<PathBuf>,
    pub entries: Vec<Entry>,
    /// The absolute filesystem path is the identity, never an index or content hash.
    pub current: Option<PathBuf>,
    kind: Option<Kind>,
    photo_meta: Option<Photo>,
    extra_rotate: u16,
    subtitle_inset: f64,
}

impl Session {
    pub fn new() -> Result<Self> {
        Ok(Self {
            mpv: create()?,
            state: PlaybackState {
                time_pos: 0.,
                duration: 0.,
                paused: false,
                volume: 100.,
                muted: false,
                speed: 1.,
                video_size: (0, 0),
                hwdec: String::new(),
                subs_visible: true,
                sub_id: String::new(),
                ended: false,
                audio: String::new(),
                rotation: 0,
                loop_file: false,
                tracks: Vec::new(),
                photo_colour: None,
                loaded: false,
            },
            folder: None,
            entries: Vec::new(),
            current: None,
            kind: None,
            photo_meta: None,
            extra_rotate: 0,
            subtitle_inset: 0.,
        })
    }

    pub fn render_handle(&self) -> Arc<Mpv> {
        self.mpv.clone()
    }
    pub fn events(&self) -> UnboundedReceiver<SessionEvent> {
        spawn_event_pump(self.mpv.clone())
    }
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }
    pub fn index(&self) -> Option<usize> {
        self.current
            .as_ref()
            .and_then(|path| self.entries.iter().position(|e| &e.path == path))
    }

    pub fn tracks_of(&self, kind: TrackKind) -> Vec<&Track> {
        self.state
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .collect()
    }

    pub fn selected_track(&self, kind: TrackKind) -> Option<&Track> {
        self.state
            .tracks
            .iter()
            .find(|t| t.kind == kind && t.selected)
    }

    /// Applies user preferences that map to live backend properties.
    pub fn apply_settings(&self, settings: &Settings) {
        let _ = self
            .mpv
            .command_async(&["set", "hwdec", settings.hwdec.mpv_value()]);
        let _ = self
            .mpv
            .command_async(&["set", "slang", settings.sub_lang.mpv_slang()]);
        let _ = self
            .mpv
            .set_property_f64_async("sub-scale", settings.sub_scale.clamp(0.5, 2.5));
        let loop_val = if settings.on_eof == OnEof::Loop {
            "inf"
        } else {
            "no"
        };
        let _ = self.mpv.command_async(&["set", "loop-file", loop_val]);
    }

    /// Loads a prepared folder and file. `resume` (seconds) applies to video only.
    pub fn open(&mut self, prepared: Prepared, resume: f64) -> Result<()> {
        let Prepared {
            folder,
            entries,
            target,
            kind,
            photo,
            options,
        } = prepared;
        self.entries = entries;
        self.folder = Some(folder);
        match target {
            Some(path) => self.load(&path, kind, photo, options, resume)?,
            None => {
                self.mpv.command_async(&["stop"])?;
                self.current = None;
                self.kind = None;
                self.photo_meta = None;
                self.extra_rotate = 0;
                self.state.photo_colour = None;
                self.state.tracks.clear();
                self.state.loaded = false;
            }
        }
        Ok(())
    }

    fn load(
        &mut self,
        path: &Path,
        kind: Option<Kind>,
        photo: Option<Photo>,
        photo_options: Option<String>,
        resume: f64,
    ) -> Result<()> {
        // Async loadfile copies argv; a path never becomes an option or command.
        let url = path.to_string_lossy();
        let kind = kind.or(Some(Kind::Video));
        let opts = match photo_options {
            Some(opts) => Some(opts),
            None if kind == Some(Kind::Video) && resume.is_finite() && resume > 0. => {
                Some(format!("start={resume:.3}"))
            }
            None => None,
        };
        self.reset_transform()?;
        let _ = self.mpv.command_async(&["set", "video-rotate", "0"]);
        match opts {
            Some(opts) => self
                .mpv
                .command_async(&["loadfile", &url, "replace", "-1", &opts])?,
            None => self.mpv.command_async(&["loadfile", &url])?,
        }
        self.current = Some(path.to_path_buf());
        self.kind = kind;
        self.state.photo_colour = photo
            .as_ref()
            .and_then(|p| p.colour)
            .map(|c| c.display_name());
        self.photo_meta = photo;
        self.extra_rotate = 0;
        self.state.ended = false;
        self.state.loaded = false;
        self.state.time_pos = 0.;
        self.state.duration = 0.;
        self.state.tracks.clear();
        Ok(())
    }

    /// Playback progress of the loaded video, for path-keyed records:
    /// `(path, position, duration, ended)`. None while loading or for photos.
    pub fn progress(&self) -> Option<(&Path, f64, f64, bool)> {
        let path = self.current.as_deref()?;
        (self.kind == Some(Kind::Video) && self.state.loaded).then_some((
            path,
            self.state.time_pos,
            self.state.duration,
            self.state.ended,
        ))
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
            Command::SeekRelative(seconds) => {
                self.mpv
                    .command_async(&["seek", &seconds.to_string(), "relative+exact"])?
            }
            Command::SeekPercent(percent) => self.mpv.command_async(&[
                "seek",
                &percent.clamp(0., 100.).to_string(),
                "absolute-percent+exact",
            ])?,
            Command::SeekPercentFast(percent) => self.mpv.command_async(&[
                "seek",
                &percent.clamp(0., 100.).to_string(),
                "absolute-percent+keyframes",
            ])?,
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
            Command::SpeedStep(dir) => {
                let next = step_speed(self.state.speed, dir);
                self.mpv.set_property_f64_async("speed", next)?;
            }
            Command::SetSpeed(speed) => self
                .mpv
                .set_property_f64_async("speed", speed.clamp(0.25, 4.))?,
            Command::ToggleSubtitles => self.mpv.command_async(&["cycle", "sub-visibility"])?,
            Command::NextSubtitle => {
                if !self.state.subs_visible {
                    self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
                }
                self.mpv.command_async(&["cycle", "sub"])?;
            }
            Command::SelectSubtitle(id) => match id {
                Some(id) => {
                    self.mpv.command_async(&["set", "sid", &id.to_string()])?;
                    self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
                }
                None => {
                    self.mpv.command_async(&["set", "sub-visibility", "no"])?;
                }
            },
            Command::LoadSubtitle(path) => {
                // `sub-add` takes a path argument via argv; never interpreted as options.
                let url = path.to_string_lossy();
                self.mpv.command_async(&["sub-add", &url, "select"])?;
                self.mpv.command_async(&["set", "sub-visibility", "yes"])?;
            }
            Command::NextAudio => self.mpv.command_async(&["cycle", "audio"])?,
            Command::SelectAudio(id) => match id {
                Some(id) => self.mpv.command_async(&["set", "aid", &id.to_string()])?,
                None => self.mpv.command_async(&["set", "aid", "no"])?,
            },
            Command::ToggleLoop => {
                let next = if self.state.loop_file { "no" } else { "inf" };
                self.mpv.command_async(&["set", "loop-file", next])?;
            }
            Command::SetLoop(enable) => {
                self.mpv
                    .command_async(&["set", "loop-file", if enable { "inf" } else { "no" }])?;
            }
            Command::RotatePhoto(delta_deg) => {
                if self.kind != Some(Kind::Photo) {
                    return Ok(());
                }
                self.extra_rotate =
                    ((self.extra_rotate as i64 + delta_deg).rem_euclid(360)) as u16;
                let is_heif = self
                    .current
                    .as_deref()
                    .and_then(|p| p.extension())
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif"));
                if is_heif {
                    let chain = self
                        .photo_meta
                        .as_ref()
                        .and_then(|p| p.vf_chain(self.extra_rotate))
                        .unwrap_or_default();
                    self.mpv.command_async(&["set", "vf", &chain])?;
                } else {
                    self.mpv.command_async(&[
                        "set",
                        "video-rotate",
                        &self.extra_rotate.to_string(),
                    ])?;
                }
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Position(v) => self.state.time_pos = *v,
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => self.state.paused = *v,
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Muted(v) => self.state.muted = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => self.state.hwdec.clone_from(v),
            SessionEvent::Subtitles(v) => self.state.subs_visible = *v,
            SessionEvent::SubtitleTrack(v) => {
                self.state.sub_id.clone_from(v);
                self.refresh_tracks();
            }
            SessionEvent::Ended(v) => self.state.ended = *v,
            SessionEvent::Audio(v) => {
                self.state.audio.clone_from(v);
                self.refresh_tracks();
            }
            SessionEvent::Rotation(v) => self.state.rotation = *v,
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::TracksChanged => self.refresh_tracks(),
            // A late event from the previous file must not mark this one loaded.
            SessionEvent::Loaded => {
                let current = self
                    .current
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned());
                self.state.loaded = self.mpv.get_property_string("path").ok() == current;
                if self.state.loaded {
                    self.refresh_tracks();
                }
            }
            _ => {}
        }
    }

    fn refresh_tracks(&mut self) {
        let count = self
            .mpv
            .get_property_string("track-list/count")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
            .min(64);
        let mut tracks = Vec::new();
        for i in 0..count {
            let Ok(ty) = self.mpv.get_property_string(&format!("track-list/{i}/type")) else {
                continue;
            };
            let kind = match ty.as_str() {
                "audio" => TrackKind::Audio,
                "sub" => TrackKind::Sub,
                _ => continue,
            };
            let id = self
                .mpv
                .get_property_string(&format!("track-list/{i}/id"))
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(i as i64 + 1);
            let title = self
                .mpv
                .get_property_string(&format!("track-list/{i}/title"))
                .unwrap_or_default();
            let lang = self
                .mpv
                .get_property_string(&format!("track-list/{i}/lang"))
                .unwrap_or_default();
            let codec = self
                .mpv
                .get_property_string(&format!("track-list/{i}/codec"))
                .unwrap_or_default();
            let selected = self
                .mpv
                .get_property_string(&format!("track-list/{i}/selected"))
                .is_ok_and(|s| s == "yes");
            let external = self
                .mpv
                .get_property_string(&format!("track-list/{i}/external"))
                .is_ok_and(|s| s == "yes");
            tracks.push(Track {
                id,
                kind,
                title,
                lang,
                codec,
                selected,
                external,
            });
        }
        self.state.tracks = tracks;
    }

    pub fn transform(&self, zoom: f64, pan: [f64; 2]) -> Result<()> {
        self.mpv.set_property_f64_async("video-zoom", zoom)?;
        self.mpv.set_property_f64_async("video-pan-x", pan[0])?;
        self.mpv.set_property_f64_async("video-pan-y", pan[1])?;
        Ok(())
    }
    fn reset_transform(&self) -> Result<()> {
        self.transform(0., [0., 0.])
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

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_property(&self, name: &str) -> Result<String> {
        Ok(self.mpv.get_property_string(name)?)
    }
    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_command(&self, args: &[&str]) -> Result<()> {
        Ok(self.mpv.command(args)?)
    }
    pub fn shutdown(&self) {
        let _ = self.mpv.command(&["quit"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_steps_are_monotonic_and_clamped() {
        assert_eq!(step_speed(1.0, 1), 1.25);
        assert_eq!(step_speed(1.0, -1), 0.75);
        assert_eq!(step_speed(3.0, 1), 3.0);
        assert_eq!(step_speed(0.25, -1), 0.25);
        assert_eq!(step_speed(1.1, 1), 1.25);
        assert_eq!(step_speed(1.1, -1), 1.0);
    }

    #[test]
    fn prepare_targets_file_or_first_entry_and_reuses_listing() {
        let dir = std::env::temp_dir().join(format!("iris-prepare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["ep10.mp4", "ep2.mp4", "notes.txt"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let folder = dir.canonicalize().unwrap();

        let opened = prepare(&dir, None).unwrap();
        assert_eq!(opened.folder, folder);
        assert_eq!(opened.entries.len(), 2);
        assert_eq!(opened.target(), Some(folder.join("ep2.mp4").as_path()));
        assert_eq!(
            (opened.kind, opened.options.as_deref()),
            (Some(Kind::Video), None)
        );

        // A cached listing of the same folder is reused as-is, even if stale.
        let cached = vec![opened.entries[1].clone()];
        let file = prepare(&dir.join("ep10.mp4"), Some((folder.clone(), cached))).unwrap();
        assert_eq!(file.target(), Some(folder.join("ep10.mp4").as_path()));
        assert_eq!(file.entries.len(), 1);
        // A listing of another folder is not.
        let other = prepare(
            &dir.join("ep10.mp4"),
            Some((PathBuf::from("/elsewhere"), Vec::new())),
        )
        .unwrap();
        assert_eq!(other.entries.len(), 2);

        assert!(prepare(&dir.join("missing.mp4"), None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn subtitle_safe_area_changes_backend_margin_and_restores_it() {
        let mut session = Session::new().unwrap();
        session
            .mpv
            .observe_property(100, "sub-margin-y-offset", Format::Int)
            .unwrap();
        let wait_for = |mpv: &Mpv, value| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if let Some(Event::PropertyChange {
                    id: 100,
                    value: Value::Int(actual),
                    ..
                }) = mpv.wait_event(0.05)
                    && actual == value
                {
                    return;
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
