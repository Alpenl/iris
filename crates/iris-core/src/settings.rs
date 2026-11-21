//! User settings, kept in a small `key = value` file:
//! macOS `~/Library/Application Support/Iris/settings.conf`,
//! Windows `%APPDATA%\Iris\settings.conf`. `IRIS_SETTINGS` overrides the path.

use std::path::{Path, PathBuf};

pub const DEFAULT_DIRECTORY_WIDTH: f32 = 288.;
pub const MIN_DIRECTORY_WIDTH: f32 = 220.;
pub const MAX_DIRECTORY_WIDTH: f32 = 520.;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// What happens when a video reaches its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OnEof {
    /// Stay on the last frame (Space replays from start).
    #[default]
    Pause,
    /// Advance to the next item in the filtered directory.
    Next,
    /// Loop the current video seamlessly.
    Loop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Hwdec {
    #[default]
    AutoSafe,
    Off,
}

impl Hwdec {
    pub fn mpv_value(self) -> &'static str {
        match self {
            Hwdec::AutoSafe => "auto-safe",
            Hwdec::Off => "no",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SubLang {
    #[default]
    ZhHans,
    ZhHant,
    En,
    Auto,
}

impl SubLang {
    pub fn mpv_slang(self) -> &'static str {
        match self {
            SubLang::ZhHans => "zh-Hans,zh-CN,chs,sc,zh,chi,zho,zh-Hant,zh-TW,cht,tc,en,eng",
            SubLang::ZhHant => "zh-Hant,zh-TW,cht,tc,zh,chi,zho,zh-Hans,zh-CN,chs,sc,en,eng",
            SubLang::En => "en,eng,zh-Hans,zh-CN,chs,sc,zh,chi,zho",
            SubLang::Auto => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VideoWheel {
    #[default]
    Volume,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PhotoDefault {
    #[default]
    Fit,
    Actual,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Which edge the directory sits on, docked and in the fullscreen popup.
    pub directory_side: Side,
    /// Width of the directory panel in logical pixels.
    pub directory_width: f32,
    /// Whether the directory starts expanded in windowed mode.
    pub directory_open: bool,
    /// Remember and restore the windowed dimensions across launches.
    pub remember_window_size: bool,
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
    pub volume: Option<f64>,
    pub speed: Option<f64>,
    /// Restore the last playback speed on launch (otherwise start at 1.0x).
    pub remember_speed: bool,
    pub on_eof: OnEof,
    /// Seek step in seconds for Left / Right keys.
    pub seek_step: f64,
    pub hwdec: Hwdec,
    /// Resume videos from their last recorded position.
    pub resume_playback: bool,
    pub sub_lang: SubLang,
    /// Subtitle font scale multiplier (mpv `sub-scale`).
    pub sub_scale: f64,
    /// Mouse wheel action over the video viewport.
    pub video_wheel: VideoWheel,
    /// Milliseconds of idle mouse before hiding fullscreen chrome.
    pub fullscreen_hide_ms: u64,
    /// Default view when opening a photo.
    pub photo_default: PhotoDefault,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            directory_side: Side::Left,
            directory_width: DEFAULT_DIRECTORY_WIDTH,
            directory_open: true,
            remember_window_size: true,
            window_width: None,
            window_height: None,
            volume: None,
            speed: None,
            remember_speed: true,
            on_eof: OnEof::Pause,
            seek_step: 5.,
            hwdec: Hwdec::AutoSafe,
            resume_playback: true,
            sub_lang: SubLang::ZhHans,
            sub_scale: 1.0,
            video_wheel: VideoWheel::Volume,
            fullscreen_hide_ms: 2000,
            photo_default: PhotoDefault::Fit,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    let dir =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Iris"));
    #[cfg(target_os = "windows")]
    let dir = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Iris"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("iris"));
    dir.map(|d| d.join("settings.conf"))
}

impl Settings {
    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut s = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return s;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("directory_side", "left") => s.directory_side = Side::Left,
                ("directory_side", "right") => s.directory_side = Side::Right,
                ("directory_width", v) => {
                    if let Ok(w) = v.parse::<f32>()
                        && w.is_finite()
                    {
                        s.directory_width = w.clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
                    }
                }
                ("directory_open", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.directory_open = b;
                    }
                }
                ("remember_window_size", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.remember_window_size = b;
                    }
                }
                ("window_width", v) => {
                    s.window_width = v
                        .parse::<f32>()
                        .ok()
                        .filter(|w| w.is_finite() && (960. ..=7680.).contains(w));
                }
                ("window_height", v) => {
                    s.window_height = v
                        .parse::<f32>()
                        .ok()
                        .filter(|h| h.is_finite() && (600. ..=4320.).contains(h));
                }
                ("volume", v) => {
                    s.volume = v
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && (0. ..=100.).contains(v));
                }
                ("speed", v) => {
                    s.speed = v
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && (0.1..=8.).contains(v));
                }
                ("remember_speed", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.remember_speed = b;
                    }
                }
                ("on_eof", "pause") => s.on_eof = OnEof::Pause,
                ("on_eof", "next") => s.on_eof = OnEof::Next,
                ("on_eof", "loop") => s.on_eof = OnEof::Loop,
                ("seek_step", v) => {
                    if let Ok(step) = v.parse::<f64>()
                        && step.is_finite()
                        && (1. ..=60.).contains(&step)
                    {
                        s.seek_step = step;
                    }
                }
                ("hwdec", "auto-safe") => s.hwdec = Hwdec::AutoSafe,
                ("hwdec", "no" | "off") => s.hwdec = Hwdec::Off,
                ("resume_playback", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.resume_playback = b;
                    }
                }
                ("sub_lang", "zh-hans") => s.sub_lang = SubLang::ZhHans,
                ("sub_lang", "zh-hant") => s.sub_lang = SubLang::ZhHant,
                ("sub_lang", "en") => s.sub_lang = SubLang::En,
                ("sub_lang", "auto") => s.sub_lang = SubLang::Auto,
                ("sub_scale", v) => {
                    if let Ok(scale) = v.parse::<f64>()
                        && scale.is_finite()
                    {
                        s.sub_scale = scale.clamp(0.5, 2.5);
                    }
                }
                ("video_wheel", "volume") => s.video_wheel = VideoWheel::Volume,
                ("video_wheel", "none") => s.video_wheel = VideoWheel::None,
                ("fullscreen_hide_ms", v) => {
                    if let Ok(ms) = v.parse::<u64>() {
                        s.fullscreen_hide_ms = ms.clamp(1000, 10000);
                    }
                }
                ("photo_default", "fit") => s.photo_default = PhotoDefault::Fit,
                ("photo_default", "actual") => s.photo_default = PhotoDefault::Actual,
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Err(e) = self.save_to(&p) {
            log::error!("saving settings to {} failed: {e}", p.display());
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let side = match self.directory_side {
            Side::Left => "left",
            Side::Right => "right",
        };
        let on_eof = match self.on_eof {
            OnEof::Pause => "pause",
            OnEof::Next => "next",
            OnEof::Loop => "loop",
        };
        let hwdec = self.hwdec.mpv_value();
        let sub_lang = match self.sub_lang {
            SubLang::ZhHans => "zh-hans",
            SubLang::ZhHant => "zh-hant",
            SubLang::En => "en",
            SubLang::Auto => "auto",
        };
        let video_wheel = match self.video_wheel {
            VideoWheel::Volume => "volume",
            VideoWheel::None => "none",
        };
        let photo_default = match self.photo_default {
            PhotoDefault::Fit => "fit",
            PhotoDefault::Actual => "actual",
        };
        let mut text = String::new();
        text.push_str(&format!("directory_side = {side}\n"));
        text.push_str(&format!(
            "directory_width = {:.0}\n",
            self.directory_width
                .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
        ));
        text.push_str(&format!("directory_open = {}\n", self.directory_open));
        text.push_str(&format!(
            "remember_window_size = {}\n",
            self.remember_window_size
        ));
        if let (Some(w), Some(h)) = (self.window_width, self.window_height)
            && w.is_finite()
            && h.is_finite()
        {
            text.push_str(&format!("window_width = {:.0}\n", w.clamp(960., 7680.)));
            text.push_str(&format!("window_height = {:.0}\n", h.clamp(600., 4320.)));
        }
        if let Some(v) = self.volume
            && v.is_finite()
        {
            text.push_str(&format!("volume = {:.0}\n", v.clamp(0., 100.)));
        }
        if let Some(v) = self.speed
            && v.is_finite()
        {
            text.push_str(&format!("speed = {:.2}\n", v.clamp(0.1, 8.)));
        }
        text.push_str(&format!("remember_speed = {}\n", self.remember_speed));
        text.push_str(&format!("on_eof = {on_eof}\n"));
        text.push_str(&format!(
            "seek_step = {:.0}\n",
            self.seek_step.clamp(1., 60.)
        ));
        text.push_str(&format!("hwdec = {hwdec}\n"));
        text.push_str(&format!("resume_playback = {}\n", self.resume_playback));
        text.push_str(&format!("sub_lang = {sub_lang}\n"));
        text.push_str(&format!(
            "sub_scale = {:.2}\n",
            self.sub_scale.clamp(0.5, 2.5)
        ));
        text.push_str(&format!("video_wheel = {video_wheel}\n"));
        text.push_str(&format!(
            "fullscreen_hide_ms = {}\n",
            self.fullscreen_hide_ms.clamp(1000, 10000)
        ));
        text.push_str(&format!("photo_default = {photo_default}\n"));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_clamps_invalid_values() {
        let dir = std::env::temp_dir().join(format!("iris-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.conf");

        let custom = Settings {
            directory_side: Side::Right,
            directory_width: 360.,
            directory_open: false,
            remember_window_size: false,
            window_width: Some(1600.),
            window_height: Some(980.),
            volume: Some(72.),
            speed: Some(1.5),
            remember_speed: false,
            on_eof: OnEof::Next,
            seek_step: 10.,
            hwdec: Hwdec::Off,
            resume_playback: false,
            sub_lang: SubLang::En,
            sub_scale: 1.25,
            video_wheel: VideoWheel::None,
            fullscreen_hide_ms: 3000,
            photo_default: PhotoDefault::Actual,
        };
        custom.save_to(&file).unwrap();
        let loaded = Settings::load_from(&file);
        assert_eq!(loaded, custom);

        std::fs::write(
            &file,
            "directory_width = 9999\nfullscreen_hide_ms = 50\nsub_scale = NaN\nvolume = 180\n",
        )
        .unwrap();
        let clamped = Settings::load_from(&file);
        assert_eq!(clamped.directory_width, MAX_DIRECTORY_WIDTH);
        assert_eq!(clamped.fullscreen_hide_ms, 1000);
        assert_eq!(clamped.sub_scale, 1.0);
        assert_eq!(clamped.volume, None);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
