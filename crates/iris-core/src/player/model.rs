//! Semantic playback models, events, commands, and remote media descriptors.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::photo::{ExifSummary, Photo};

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;
pub const SPEED_STEPS: &[f64] = &[
    MIN_SPEED, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, MAX_SPEED,
];

pub fn step_speed(current: f64, dir: isize) -> f64 {
    if dir > 0 {
        SPEED_STEPS
            .iter()
            .copied()
            .find(|&s| s > current + 0.01)
            .unwrap_or(MAX_SPEED)
    } else if dir < 0 {
        SPEED_STEPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < current - 0.01)
            .unwrap_or(MIN_SPEED)
    } else {
        current
    }
}

pub fn format_time(t: f64) -> String {
    let total = t.max(0.) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Semantic player command preconditions for clean localized UI presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerActionError {
    NotSeekable,
    InvalidAbRange,
    NoVisualMedia,
    InvalidSubtitlePath,
    SubtitleFileNotFound,
    /// mpv takes UTF-8 paths; a lossy conversion would name a different file.
    NonUnicodePath,
}

impl std::fmt::Display for PlayerActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSeekable => f.write_str("A-B loop requires seekable media"),
            Self::InvalidAbRange => {
                f.write_str("A-B loop requires a finite range in current seekable media")
            }
            Self::NoVisualMedia => f.write_str("LUT preview requires loaded visual media"),
            Self::InvalidSubtitlePath => f.write_str(
                "remote or internal mpv protocols are not allowed in local subtitle path",
            ),
            Self::SubtitleFileNotFound => {
                f.write_str("subtitle path does not exist or is not a regular file")
            }
            Self::NonUnicodePath => f.write_str("path is not valid Unicode and cannot reach mpv"),
        }
    }
}

impl std::error::Error for PlayerActionError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectMode {
    #[default]
    Original,
    Ratio16x9,
    Ratio4x3,
    Ratio21x9,
    FillCrop,
}

impl AspectMode {
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

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub time: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

/// External subtitle attachment for [`RemoteMediaRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSubtitle {
    /// HTTP(S) subtitle without independent custom headers, loaded through the relay.
    Url(String),
    /// Pre-fetched subtitle text loaded via `sub-add` `"memory://<content>"`,
    /// isolating third-party or custom-header subtitles from main stream credentials.
    InlineText(String),
}

pub(super) fn fetch_subtitle(
    url: &url::Url,
    headers: &BTreeMap<String, String>,
    credentials: &url::Url,
    scope: &iris_base::net::http_client::NetworkScope,
    cancel: &iris_base::net::http_client::HttpCancel,
) -> Result<String> {
    let body = iris_base::net::http_client::fetch_subtitle_bytes(
        url.clone(),
        headers,
        credentials,
        scope,
        cancel,
    )?;
    let mut text = String::from_utf8(body).context("subtitle payload is not UTF-8")?;
    let start = text.len() - subtitle_text(&text)?.len();
    text.drain(..start);
    Ok(text)
}

pub(super) fn subtitle_text(text: &str) -> Result<&str> {
    let text = text.trim_start_matches('\u{feff}');
    let header = text.trim_start();
    let starts = |magic: &str| {
        header
            .get(..magic.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(magic))
    };
    if header.is_empty()
        || text.len() > 8 * 1024 * 1024
        || text.contains('\0')
        || ["# mpv EDL", "#EXTM3U", "[playlist]", "<asx", "<?xml"]
            .into_iter()
            .any(starts)
    {
        bail!("invalid external subtitle text");
    }
    Ok(text)
}

/// Request descriptor for opening a direct HTTP(S) or plugin-resolved media stream.
#[derive(Clone, Debug)]
pub struct RemoteMediaRequest {
    pub uri: MediaUri,
    pub title: String,
    pub playback_url: String,
    pub headers: BTreeMap<String, String>,
    pub kind: Kind,
    pub is_live: bool,
    pub photo: Option<Photo>,
    pub subtitles: Vec<RemoteSubtitle>,
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

/// Read-only backend diagnostic snapshot exposed for integration verification.
#[derive(Clone, Debug, Default)]
pub struct BackendDiagnostics {
    pub mpv_version: String,
    pub ffmpeg_version: String,
    pub access_references: String,
    pub demuxer: String,
    pub file_format: String,
    pub vf: String,
    pub out_pixel_format: String,
    pub avsync: Option<f64>,
    pub track_count: u32,
    pub paused: bool,
}

#[derive(Debug)]
pub enum SessionEvent {
    Seek,
    CommandReply { id: u64, error: i32 },
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
    SourceRotation(i64),
    LoopFile(bool),
    AbStart(Option<f64>),
    AbEnd(Option<f64>),
    Chapter(i64),
    ChaptersChanged,
    TracksChanged,
    Buffering(bool),
    CacheDuration(f64),
    Loaded,
    Reconfigured,
    QueueOverflow,
    Error(i32),
}

/// Event metadata retained from mpv's ordered event stream.
#[derive(Debug)]
pub struct SessionUpdate {
    pub(super) playlist_entry_id: Option<i64>,
    pub(super) event: SessionEvent,
}

#[derive(Clone, Debug)]
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
    pub sub_delay: f64,
    pub ended: bool,
    pub audio: String,
    pub rotation: i64,
    pub loop_file: bool,
    pub ab_start: Option<f64>,
    pub ab_end: Option<f64>,
    pub aspect: AspectMode,
    pub hflip: bool,
    pub vflip: bool,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub current_chapter: i64,
    pub photo_colour: Option<&'static str>,
    pub photo_exif: Option<ExifSummary>,
    pub loaded: bool,
    pub buffering: bool,
    pub cache_duration: f64,
    pub is_live: bool,
    pub eq_brightness: i64,
    pub eq_contrast: i64,
    pub eq_saturation: i64,
    pub eq_gamma: i64,
    pub audio_device: String,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            time_pos: 0.,
            duration: 0.,
            paused: false,
            volume: 100.,
            muted: false,
            speed: 1.,
            video_size: (0, 0),
            hwdec: String::new(),
            subs_visible: true,
            sub_delay: 0.,
            ended: false,
            audio: String::new(),
            rotation: 0,
            loop_file: false,
            ab_start: None,
            ab_end: None,
            aspect: AspectMode::Original,
            hflip: false,
            vflip: false,
            tracks: Vec::new(),
            chapters: Vec::new(),
            current_chapter: -1,
            photo_colour: None,
            photo_exif: None,
            loaded: false,
            buffering: false,
            cache_duration: 0.,
            is_live: false,
            eq_brightness: 0,
            eq_contrast: 0,
            eq_saturation: 0,
            eq_gamma: 0,
            audio_device: "auto".into(),
        }
    }
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
    SetSpeed(f64),
    ToggleSubtitles,
    NextSubtitle,
    SelectSubtitle(Option<i64>),
    SubSeekPrev,
    SubSeekNext,
    SubDelayBy(f64),
    SetSubDelay(f64),
    LoadSubtitle(PathBuf),
    NextAudio,
    SelectAudio(Option<i64>),
    SetAudioDevice(String),
    SetLoop(bool),
    SetAbLoop {
        start: f64,
        end: f64,
    },
    CycleAbLoop,
    ClearAbLoop,
    SetAspect(AspectMode),
    SetVideoEq {
        brightness: i64,
        contrast: i64,
        saturation: i64,
        gamma: i64,
    },
    ResetVideoEq,
    RotateMedia(i64),
    ToggleHFlip,
    ToggleVFlip,
    NextChapter,
    PrevChapter,
    SelectChapter(usize),
    Screenshot {
        path: PathBuf,
        with_subs: bool,
    },
}
