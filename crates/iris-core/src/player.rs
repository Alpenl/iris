//! Viewport-owned playback session. All client commands and backend names live here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv, Value};

use crate::library::{self, Entry, FolderEntry, Kind, ScanResult};
use crate::photo::{self, ExifSummary, Photo};
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

pub fn friendly_error(code: i32) -> &'static str {
    match code {
        -13 => "无法打开或读取文件",
        -14 => "未识别的媒体格式",
        -9 | -10 => "不支持的解码格式或操作",
        -1 => "事件队列溢出",
        -2 => "内存不足",
        _ => "解码或播放失败",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AspectMode {
    #[default]
    Original,
    Ratio16x9,
    Ratio4x3,
    Ratio21x9,
    FillCrop,
}

impl AspectMode {
    pub fn label(self) -> &'static str {
        match self {
            AspectMode::Original => "原始比例",
            AspectMode::Ratio16x9 => "16:9",
            AspectMode::Ratio4x3 => "4:3",
            AspectMode::Ratio21x9 => "21:9",
            AspectMode::FillCrop => "铺满裁剪",
        }
    }

    pub fn next(self) -> Self {
        match self {
            AspectMode::Original => AspectMode::Ratio16x9,
            AspectMode::Ratio16x9 => AspectMode::Ratio4x3,
            AspectMode::Ratio4x3 => AspectMode::Ratio21x9,
            AspectMode::Ratio21x9 => AspectMode::FillCrop,
            AspectMode::FillCrop => AspectMode::Original,
        }
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

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub time: f64,
}

#[derive(Clone, Debug, Default)]
pub struct MediaStats {
    pub file_name: String,
    pub file_size: u64,
    pub file_format: String,
    pub resolution: Option<(i64, i64)>,
    pub video_codec: String,
    pub video_fps: Option<f64>,
    pub video_bitrate: Option<f64>,
    pub pixel_format: String,
    pub hwdec: String,
    pub dropped_frames: Option<i64>,
    pub audio_codec: String,
    pub audio_channels: String,
    pub audio_sample_rate: Option<i64>,
    pub audio_bitrate: Option<f64>,
    pub photo_colour: Option<&'static str>,
    pub photo_exif: Option<ExifSummary>,
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
    pub const SUB_DELAY: u64 = 17;
    pub const CHAPTER: u64 = 18;
    pub const CHAPTER_COUNT: u64 = 19;
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
        // Auto-load matching external subtitle files (e.g. movie.chs.ass).
        ("sub-auto", "fuzzy"),
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
        (prop::SUB_DELAY, "sub-delay", Format::Double),
        (prop::CHAPTER, "chapter", Format::Int),
        (prop::CHAPTER_COUNT, "chapter-list/count", Format::Int),
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
    SubDelay(f64),
    Ended(bool),
    Audio(String),
    Rotation(i64),
    LoopFile(bool),
    Chapter(i64),
    ChaptersChanged,
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
    pub sub_delay: f64,
    pub ended: bool,
    pub audio: String,
    pub rotation: i64,
    pub loop_file: bool,
    pub aspect: AspectMode,
    pub hflip: bool,
    pub vflip: bool,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub current_chapter: i64,
    pub photo_colour: Option<&'static str>,
    pub photo_exif: Option<ExifSummary>,
    /// The current path has finished loading; positions now belong to it.
    pub loaded: bool,
}

pub enum Command {
    TogglePlayback,
    SeekRelative(f64),
    SeekAbsolute(f64),
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
    SubDelayBy(f64),
    SetSubDelay(f64),
    LoadSubtitle(PathBuf),
    NextAudio,
    SelectAudio(Option<i64>),
    ToggleLoop,
    SetLoop(bool),
    SetAspect(AspectMode),
    RotateMedia(i64),
    ToggleHFlip,
    ToggleVFlip,
    NextChapter,
    PrevChapter,
    SelectChapter(usize),
    Screenshot { path: PathBuf, with_subs: bool },
}

/// A folder scan and photo probe done off the UI thread, ready to load.
pub struct Prepared {
    folder: PathBuf,
    entries: Vec<Entry>,
    subfolders: Vec<FolderEntry>,
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
pub fn prepare(path: &Path, cached: Option<(PathBuf, ScanResult)>) -> Result<Prepared> {
    let path = path.canonicalize()?;
    let (folder, file) = if path.is_dir() {
        (path, None)
    } else {
        (
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Some(path),
        )
    };
    let ScanResult {
        entries,
        subfolders,
    } = match cached {
        Some((dir, scan)) if dir == folder => scan,
        _ => library::scan_dir(&folder),
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
        subfolders,
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
    pub subfolders: Vec<FolderEntry>,
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
                sub_delay: 0.,
                ended: false,
                audio: String::new(),
                rotation: 0,
                loop_file: false,
                aspect: AspectMode::Original,
                hflip: false,
                vflip: false,
                tracks: Vec::new(),
                chapters: Vec::new(),
                current_chapter: -1,
                photo_colour: None,
                photo_exif: None,
                loaded: false,
            },
            folder: None,
            entries: Vec::new(),
            subfolders: Vec::new(),
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
    pub fn is_actively_playing_video(&self) -> bool {
        self.kind == Some(Kind::Video) && self.state.loaded && !self.state.paused && !self.state.ended
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

    pub fn chapter_at(&self, time: f64) -> Option<&Chapter> {
        self.state
            .chapters
            .iter()
            .rfind(|c| c.time <= time + 0.05)
    }

    /// Queries detailed media / decoder stats on demand (only when Info panel is open).
    pub fn media_stats(&self) -> MediaStats {
        let path = self.current.as_deref();
        let file_name = path
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file_size = path
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        let prop_str = |name: &str| self.mpv.get_property_string(name).unwrap_or_default();
        let prop_f64 = |name: &str| self.mpv.get_property_f64(name).ok().filter(|v| v.is_finite());
        let prop_i64 = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
        };
        let (w, h) = self.state.video_size;
        MediaStats {
            file_name,
            file_size,
            file_format: prop_str("file-format"),
            resolution: (w > 0 && h > 0).then_some((w, h)),
            video_codec: prop_str("video-codec"),
            video_fps: prop_f64("estimated-vf-fps").or_else(|| prop_f64("container-fps")),
            video_bitrate: prop_f64("video-bitrate"),
            pixel_format: prop_str("video-params/pixelformat"),
            hwdec: self.state.hwdec.clone(),
            dropped_frames: prop_i64("frame-drop-count"),
            audio_codec: prop_str("audio-codec-name"),
            audio_channels: prop_str("audio-params/channels"),
            audio_sample_rate: prop_i64("audio-params/samplerate"),
            audio_bitrate: prop_f64("audio-bitrate"),
            photo_colour: self.state.photo_colour,
            photo_exif: self.state.photo_exif.clone(),
        }
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
            subfolders,
            target,
            kind,
            photo,
            options,
        } = prepared;
        self.entries = entries;
        self.subfolders = subfolders;
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
                self.state.photo_exif = None;
                self.state.tracks.clear();
                self.state.chapters.clear();
                self.state.loaded = false;
            }
        }
        Ok(())
    }

    /// Opens an explicit multi-file drop queue as the active playlist.
    pub fn open_queue(&mut self, entries: Vec<Entry>, resume: f64) -> Result<()> {
        let Some(first) = entries.first().cloned() else {
            return Ok(());
        };
        let folder = first
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let photo = (first.kind == Kind::Photo).then(|| photo::probe(&first.path));
        let options = photo.as_ref().and_then(Photo::mpv_options);
        self.folder = Some(folder);
        self.entries = entries;
        self.subfolders.clear();
        self.load(&first.path, Some(first.kind), photo, options, resume)
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
            None if matches!(kind, Some(Kind::Video | Kind::Audio))
                && resume.is_finite()
                && resume > 0. =>
            {
                Some(format!("start={resume:.3}"))
            }
            None => None,
        };
        self.reset_transform()?;
        let _ = self.mpv.command_async(&["set", "video-rotate", "0"]);
        let _ = self.mpv.command_async(&["set", "video-aspect-override", "-1"]);
        let _ = self.mpv.set_property_f64_async("panscan", 0.);
        let _ = self.mpv.set_property_f64_async("sub-delay", 0.);
        let _ = self.mpv.command_async(&["set", "vf", ""]);
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
        self.state.photo_exif = photo.as_ref().and_then(|p| p.exif.clone());
        self.photo_meta = photo;
        self.extra_rotate = 0;
        self.state.aspect = AspectMode::Original;
        self.state.hflip = false;
        self.state.vflip = false;
        self.state.sub_delay = 0.;
        self.state.ended = false;
        self.state.loaded = false;
        self.state.time_pos = 0.;
        self.state.duration = 0.;
        self.state.tracks.clear();
        self.state.chapters.clear();
        self.state.current_chapter = -1;
        Ok(())
    }

    /// Playback progress of the loaded video/audio, for path-keyed records:
    /// `(path, position, duration, ended)`. None while loading or for photos.
    pub fn progress(&self) -> Option<(&Path, f64, f64, bool)> {
        let path = self.current.as_deref()?;
        (matches!(self.kind, Some(Kind::Video | Kind::Audio)) && self.state.loaded).then_some((
            path,
            self.state.time_pos,
            self.state.duration,
            self.state.ended,
        ))
    }

    fn apply_vf_filters(&self) -> Result<()> {
        let mut chain = Vec::new();
        if self.kind == Some(Kind::Photo) {
            let is_heif = self
                .current
                .as_deref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif"));
            if let Some(meta) = &self.photo_meta
                && let Some(base) = meta.vf_chain(if is_heif { self.extra_rotate } else { 0 })
            {
                chain.push(base);
            }
        }
        if self.state.hflip {
            chain.push("hflip".into());
        }
        if self.state.vflip {
            chain.push("vflip".into());
        }
        let joined = chain.join(",");
        self.mpv.command_async(&["set", "vf", &joined])?;
        Ok(())
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
            Command::SeekAbsolute(seconds) => self.mpv.command_async(&[
                "seek",
                &seconds.max(0.).to_string(),
                "absolute+exact",
            ])?,
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
            Command::SubDelayBy(delta) => {
                let next = ((self.state.sub_delay + delta) * 100.).round() / 100.;
                self.mpv
                    .set_property_f64_async("sub-delay", next.clamp(-60., 60.))?;
            }
            Command::SetSubDelay(delay) => {
                self.mpv
                    .set_property_f64_async("sub-delay", delay.clamp(-60., 60.))?;
            }
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
            Command::SetAspect(mode) => {
                self.state.aspect = mode;
                match mode {
                    AspectMode::Original => {
                        self.mpv
                            .command_async(&["set", "video-aspect-override", "-1"])?;
                        self.mpv.set_property_f64_async("panscan", 0.)?;
                    }
                    AspectMode::Ratio16x9 => {
                        self.mpv
                            .command_async(&["set", "video-aspect-override", "16:9"])?;
                        self.mpv.set_property_f64_async("panscan", 0.)?;
                    }
                    AspectMode::Ratio4x3 => {
                        self.mpv
                            .command_async(&["set", "video-aspect-override", "4:3"])?;
                        self.mpv.set_property_f64_async("panscan", 0.)?;
                    }
                    AspectMode::Ratio21x9 => {
                        self.mpv
                            .command_async(&["set", "video-aspect-override", "21:9"])?;
                        self.mpv.set_property_f64_async("panscan", 0.)?;
                    }
                    AspectMode::FillCrop => {
                        self.mpv
                            .command_async(&["set", "video-aspect-override", "-1"])?;
                        self.mpv.set_property_f64_async("panscan", 1.)?;
                    }
                }
            }
            Command::RotateMedia(delta_deg) => {
                if !matches!(self.kind, Some(Kind::Photo | Kind::Video)) {
                    return Ok(());
                }
                self.extra_rotate =
                    ((self.extra_rotate as i64 + delta_deg).rem_euclid(360)) as u16;
                let is_heif = self.kind == Some(Kind::Photo)
                    && self
                        .current
                        .as_deref()
                        .and_then(|p| p.extension())
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| {
                            matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif")
                        });
                if is_heif {
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
            Command::NextChapter => {
                self.mpv.command_async(&["add", "chapter", "1"])?;
            }
            Command::PrevChapter => {
                self.mpv.command_async(&["add", "chapter", "-1"])?;
            }
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
            SessionEvent::SubDelay(v) => self.state.sub_delay = *v,
            SessionEvent::Ended(v) => self.state.ended = *v,
            SessionEvent::Audio(v) => {
                self.state.audio.clone_from(v);
                self.refresh_tracks();
            }
            SessionEvent::Rotation(v) => self.state.rotation = *v,
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::Chapter(v) => self.state.current_chapter = *v,
            SessionEvent::ChaptersChanged => self.refresh_chapters(),
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
                    self.refresh_chapters();
                }
            }
            _ => {}
        }
    }

    fn refresh_chapters(&mut self) {
        let count = self
            .mpv
            .get_property_string("chapter-list/count")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
            .min(128);
        let mut chapters = Vec::with_capacity(count);
        for i in 0..count {
            let time = self
                .mpv
                .get_property_f64(&format!("chapter-list/{i}/time"))
                .unwrap_or(0.);
            let title = self
                .mpv
                .get_property_string(&format!("chapter-list/{i}/title"))
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("第 {} 章", i + 1));
            chapters.push(Chapter { title, time });
        }
        self.state.chapters = chapters;
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
        let cached = ScanResult {
            entries: vec![opened.entries[1].clone()],
            subfolders: Vec::new(),
        };
        let file = prepare(&dir.join("ep10.mp4"), Some((folder.clone(), cached))).unwrap();
        assert_eq!(file.target(), Some(folder.join("ep10.mp4").as_path()));
        assert_eq!(file.entries.len(), 1);
        // A listing of another folder is not.
        let other = prepare(
            &dir.join("ep10.mp4"),
            Some((PathBuf::from("/elsewhere"), ScanResult::default())),
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
