//! Semantic playback models, events, commands, and remote media descriptors.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::photo::{ExifSummary, Photo};

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

pub fn format_time(t: f64) -> String {
    let total = t.max(0.) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

/// External subtitle attachment for [`RemoteMediaRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSubtitle {
    /// Direct HTTP(S) subtitle URL without independent custom headers.
    /// Attached via `sub-files-append` when same-origin with the primary URL
    /// (or when the primary stream carries no custom headers).
    Url(String),
    /// Pre-fetched subtitle text loaded via `sub-add` `"memory://<content>"`,
    /// isolating third-party or custom-header subtitles from main stream credentials.
    InlineText(String),
}

impl RemoteSubtitle {
    /// Prefetches a remote subtitle (up to 8 MiB) off the UI thread into [`RemoteSubtitle::InlineText`].
    pub fn prefetch(url: &str, headers: &BTreeMap<String, String>) -> Result<Self> {
        const MAX_SUBTITLE_BYTES: usize = 8 * 1024 * 1024;
        let resp =
            crate::vfs::http_client::request_bytes("GET", url, headers, None, MAX_SUBTITLE_BYTES)?;
        if !(200..300).contains(&resp.status) {
            bail!("subtitle HTTP fetch failed with status {}", resp.status);
        }
        let text = String::from_utf8(resp.body).context("subtitle payload is not UTF-8")?;
        Ok(Self::InlineText(text))
    }
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
    Buffering(bool),
    CacheDuration(f64),
    Loaded,
    Reconfigured,
    Error(i32),
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
    SubDelayBy(f64),
    SetSubDelay(f64),
    LoadSubtitle(PathBuf),
    NextAudio,
    SelectAudio(Option<i64>),
    SetAudioDevice(String),
    SetLoop(bool),
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
