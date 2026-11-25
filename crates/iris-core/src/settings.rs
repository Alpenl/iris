//! User settings, kept in a small `key = value` file:
//! macOS `~/Library/Application Support/Iris/settings.conf`,
//! Windows `%APPDATA%\Iris\settings.conf`. `IRIS_SETTINGS` overrides the path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;
use crate::library::{SortBy, SortOrder};
use crate::player::{MAX_SPEED, MIN_SPEED};

pub const DEFAULT_DIRECTORY_WIDTH: f32 = 288.;
pub const MIN_DIRECTORY_WIDTH: f32 = 220.;
pub const MAX_DIRECTORY_WIDTH: f32 = 520.;

pub const DEFAULT_UI_FONT_SCALE: f32 = 1.0;
pub const MIN_UI_FONT_SCALE: f32 = 0.85;
pub const MAX_UI_FONT_SCALE: f32 = 1.35;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// What happens when a video reaches its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnEof {
    /// Stay on the last frame (Space replays from start).
    #[default]
    Pause,
    /// Advance to the next item in the filtered directory.
    Next,
    /// Loop the current video seamlessly.
    Loop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hwdec {
    #[default]
    AutoSafe,
    Off,
}

impl Hwdec {
    pub(crate) fn mpv_value(self) -> &'static str {
        match self {
            Hwdec::AutoSafe => "auto-safe",
            Hwdec::Off => "no",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubLang {
    #[default]
    ZhHans,
    ZhHant,
    En,
    Auto,
}

impl SubLang {
    pub(crate) fn mpv_slang(self) -> &'static str {
        match self {
            SubLang::ZhHans => "zh-Hans,zh-CN,chs,sc,zh,chi,zho,zh-Hant,zh-TW,cht,tc,en,eng",
            SubLang::ZhHant => "zh-Hant,zh-TW,cht,tc,zh,chi,zho,zh-Hans,zh-CN,chs,sc,en,eng",
            SubLang::En => "en,eng,zh-Hans,zh-CN,chs,sc,zh,chi,zho",
            SubLang::Auto => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoWheel {
    #[default]
    Volume,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Single-click on the video viewport pauses / resumes playback.
    pub single_click_pause: bool,
    /// Double-clicking a photo toggles 1:1 actual size (otherwise fullscreen).
    pub photo_double_click_actual: bool,
    /// Milliseconds of idle mouse before hiding fullscreen chrome.
    pub fullscreen_hide_ms: u64,
    /// Default view when opening a photo.
    pub photo_default: PhotoDefault,
    /// Directory sort criterion and direction.
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    /// Include subtitles when taking screenshots.
    pub screenshot_subs: bool,
    /// Keep window floating above other apps.
    pub always_on_top: bool,
    /// Show bottom thumbnail filmstrip when browsing photos.
    pub photo_filmstrip: bool,
    /// Plugin IDs explicitly disabled by the user.
    pub disabled_plugins: Vec<String>,
    /// Active UI color/chrome theme ID (built-in or plugin-provided).
    pub theme_id: String,
    /// Active workspace layout/scene preset ID.
    pub active_preset: String,
    /// Render the control bar as a floating dock even in windowed mode.
    pub floating_dock: bool,
    /// Show media resolution and codec metadata in the titlebar.
    pub show_titlebar_meta: bool,
    /// Prefer compact control bar density.
    pub compact_controls: bool,
    /// Built-in dock button IDs hidden from the control bar.
    pub hidden_dock_buttons: Vec<String>,
    /// Preferred audio output device name (`"auto"` by default).
    pub audio_device: String,
    /// Global UI typography & control height scale multiplier (`0.85`..=`1.35`).
    pub ui_font_scale: f32,
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
            single_click_pause: true,
            photo_double_click_actual: true,
            fullscreen_hide_ms: 2000,
            photo_default: PhotoDefault::Fit,
            sort_by: SortBy::Name,
            sort_order: SortOrder::Asc,
            screenshot_subs: true,
            always_on_top: false,
            photo_filmstrip: true,
            disabled_plugins: Vec::new(),
            theme_id: "graphite".into(),
            active_preset: "default".into(),
            floating_dock: false,
            show_titlebar_meta: true,
            compact_controls: false,
            hidden_dock_buttons: Vec::new(),
            audio_device: "auto".into(),
            ui_font_scale: DEFAULT_UI_FONT_SCALE,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    fs_util::data_dir().map(|d| d.join("settings.conf"))
}

pub fn data_dir() -> Option<PathBuf> {
    fs_util::data_dir()
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
                        .filter(|w| w.is_finite() && (480. ..=7680.).contains(w));
                }
                ("window_height", v) => {
                    s.window_height = v
                        .parse::<f32>()
                        .ok()
                        .filter(|h| h.is_finite() && (300. ..=4320.).contains(h));
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
                        .filter(|v| v.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(v));
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
                ("single_click_pause", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.single_click_pause = b;
                    }
                }
                ("photo_double_click_actual", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.photo_double_click_actual = b;
                    }
                }
                ("fullscreen_hide_ms", v) => {
                    if let Ok(ms) = v.parse::<u64>() {
                        s.fullscreen_hide_ms = ms.clamp(1000, 10000);
                    }
                }
                ("photo_default", "fit") => s.photo_default = PhotoDefault::Fit,
                ("photo_default", "actual") => s.photo_default = PhotoDefault::Actual,
                ("sort_by", "name") => s.sort_by = SortBy::Name,
                ("sort_by", "modified") => s.sort_by = SortBy::Modified,
                ("sort_by", "size") => s.sort_by = SortBy::Size,
                ("sort_order", "asc") => s.sort_order = SortOrder::Asc,
                ("sort_order", "desc") => s.sort_order = SortOrder::Desc,
                ("screenshot_subs", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.screenshot_subs = b;
                    }
                }
                ("always_on_top", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.always_on_top = b;
                    }
                }
                ("photo_filmstrip", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.photo_filmstrip = b;
                    }
                }
                ("disabled_plugins", v) => {
                    s.disabled_plugins =
                        v.split(',').filter_map(list_id).map(String::from).collect();
                }
                ("theme_id", v) => {
                    let clean = sanitize_line_value(v);
                    if !clean.is_empty() {
                        s.theme_id = clean;
                    }
                }
                ("active_preset", v) => {
                    let clean = sanitize_line_value(v);
                    if !clean.is_empty() {
                        s.active_preset = clean;
                    }
                }
                ("floating_dock", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.floating_dock = b;
                    }
                }
                ("show_titlebar_meta", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.show_titlebar_meta = b;
                    }
                }
                ("compact_controls", v) => {
                    if let Some(b) = parse_bool(v) {
                        s.compact_controls = b;
                    }
                }
                ("hidden_dock_buttons", v) => {
                    s.hidden_dock_buttons =
                        v.split(',').filter_map(list_id).map(String::from).collect();
                }
                ("audio_device", v) => {
                    let clean = sanitize_line_value(v);
                    if !clean.is_empty() {
                        s.audio_device = clean;
                    }
                }
                ("ui_font_scale", v) => {
                    if let Ok(scale) = v.parse::<f32>()
                        && scale.is_finite()
                    {
                        s.ui_font_scale = scale.clamp(MIN_UI_FONT_SCALE, MAX_UI_FONT_SCALE);
                    }
                }
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
        let sort_by = match self.sort_by {
            SortBy::Name => "name",
            SortBy::Modified => "modified",
            SortBy::Size => "size",
        };
        let sort_order = match self.sort_order {
            SortOrder::Asc => "asc",
            SortOrder::Desc => "desc",
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
            text.push_str(&format!("window_width = {:.0}\n", w.clamp(480., 7680.)));
            text.push_str(&format!("window_height = {:.0}\n", h.clamp(300., 4320.)));
        }
        if let Some(v) = self.volume
            && v.is_finite()
        {
            text.push_str(&format!("volume = {:.0}\n", v.clamp(0., 100.)));
        }
        if let Some(v) = self.speed
            && v.is_finite()
        {
            text.push_str(&format!("speed = {:.2}\n", v.clamp(MIN_SPEED, MAX_SPEED)));
        }
        text.push_str(&format!("remember_speed = {}\n", self.remember_speed));
        text.push_str(&format!("on_eof = {on_eof}\n"));
        let step = if self.seek_step.is_finite() {
            (self.seek_step.clamp(1., 60.) * 100.).round() / 100.
        } else {
            5.
        };
        if step.fract() == 0. {
            text.push_str(&format!("seek_step = {step:.0}\n"));
        } else {
            text.push_str(&format!("seek_step = {step}\n"));
        }
        text.push_str(&format!("hwdec = {hwdec}\n"));
        text.push_str(&format!("resume_playback = {}\n", self.resume_playback));
        text.push_str(&format!("sub_lang = {sub_lang}\n"));
        text.push_str(&format!(
            "sub_scale = {:.2}\n",
            self.sub_scale.clamp(0.5, 2.5)
        ));
        text.push_str(&format!("video_wheel = {video_wheel}\n"));
        text.push_str(&format!(
            "single_click_pause = {}\n",
            self.single_click_pause
        ));
        text.push_str(&format!(
            "photo_double_click_actual = {}\n",
            self.photo_double_click_actual
        ));
        text.push_str(&format!(
            "fullscreen_hide_ms = {}\n",
            self.fullscreen_hide_ms.clamp(1000, 10000)
        ));
        text.push_str(&format!("photo_default = {photo_default}\n"));
        text.push_str(&format!("sort_by = {sort_by}\n"));
        text.push_str(&format!("sort_order = {sort_order}\n"));
        text.push_str(&format!("screenshot_subs = {}\n", self.screenshot_subs));
        text.push_str(&format!("always_on_top = {}\n", self.always_on_top));
        text.push_str(&format!("photo_filmstrip = {}\n", self.photo_filmstrip));
        write_id_list(&mut text, "disabled_plugins", &self.disabled_plugins);
        let theme_id = sanitize_line_value(&self.theme_id);
        text.push_str(&format!(
            "theme_id = {}\n",
            if theme_id.is_empty() {
                "graphite"
            } else {
                &theme_id
            }
        ));
        let active_preset = sanitize_line_value(&self.active_preset);
        text.push_str(&format!(
            "active_preset = {}\n",
            if active_preset.is_empty() {
                "default"
            } else {
                &active_preset
            }
        ));
        text.push_str(&format!("floating_dock = {}\n", self.floating_dock));
        text.push_str(&format!(
            "show_titlebar_meta = {}\n",
            self.show_titlebar_meta
        ));
        text.push_str(&format!("compact_controls = {}\n", self.compact_controls));
        write_id_list(&mut text, "hidden_dock_buttons", &self.hidden_dock_buttons);
        let audio_device = sanitize_line_value(&self.audio_device);
        text.push_str(&format!(
            "audio_device = {}\n",
            if audio_device.is_empty() {
                "auto"
            } else {
                &audio_device
            }
        ));
        let ui_font_scale = if self.ui_font_scale.is_finite() {
            self.ui_font_scale
                .clamp(MIN_UI_FONT_SCALE, MAX_UI_FONT_SCALE)
        } else {
            DEFAULT_UI_FONT_SCALE
        };
        text.push_str(&format!("ui_font_scale = {ui_font_scale:.2}\n"));

        fs_util::atomic_write(path, &text)
    }
}

fn list_id(id: &str) -> Option<&str> {
    if id.contains(',') || id.chars().any(char::is_control) {
        return None;
    }
    let id = id.trim();
    (!id.is_empty()).then_some(id)
}

fn write_id_list(text: &mut String, key: &str, ids: &[String]) {
    let mut ids = ids.iter().filter_map(|id| list_id(id));
    if let Some(first) = ids.next() {
        text.push_str(key);
        text.push_str(" = ");
        text.push_str(first);
        for id in ids {
            text.push(',');
            text.push_str(id);
        }
        text.push('\n');
    }
}

fn sanitize_line_value(v: &str) -> String {
    v.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
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
        let dir = crate::test_util::TestDir::new("settings");
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
            single_click_pause: false,
            photo_double_click_actual: false,
            fullscreen_hide_ms: 3000,
            photo_default: PhotoDefault::Actual,
            sort_by: SortBy::Modified,
            sort_order: SortOrder::Desc,
            screenshot_subs: false,
            always_on_top: true,
            photo_filmstrip: false,
            disabled_plugins: vec!["foo".into(), "bar".into()],
            theme_id: "oled".into(),
            active_preset: "cinema".into(),
            floating_dock: true,
            show_titlebar_meta: false,
            compact_controls: true,
            hidden_dock_buttons: vec!["screenshot".into(), "loop".into()],
            audio_device: "coreaudio/BuiltInSpeakerDevice".into(),
            ui_font_scale: 1.15,
        };
        custom.save_to(&file).unwrap();
        let loaded = Settings::load_from(&file);
        assert_eq!(loaded, custom);

        std::fs::write(
            &file,
            "directory_width = 9999\nfullscreen_hide_ms = 50\nsub_scale = NaN\nvolume = 180\ntheme_id =   \naudio_device = \nui_font_scale = 9.5\n",
        )
        .unwrap();
        let clamped = Settings::load_from(&file);
        assert_eq!(clamped.directory_width, MAX_DIRECTORY_WIDTH);
        assert_eq!(clamped.fullscreen_hide_ms, 1000);
        assert_eq!(clamped.sub_scale, 1.0);
        assert_eq!(clamped.volume, None);
        assert_eq!(clamped.theme_id, "graphite");
        assert_eq!(clamped.active_preset, "default");
        assert!(!clamped.floating_dock);
        assert!(clamped.show_titlebar_meta);
        assert!(!clamped.compact_controls);
        assert!(clamped.hidden_dock_buttons.is_empty());
        assert_eq!(clamped.audio_device, "auto");
        assert_eq!(clamped.ui_font_scale, MAX_UI_FONT_SCALE);

        std::fs::write(&file, "ui_font_scale = NaN\n").unwrap();
        assert_eq!(
            Settings::load_from(&file).ui_font_scale,
            DEFAULT_UI_FONT_SCALE
        );

        // Verify serde snake_case serialization for preset-shared enums.
        let json = serde_json::to_string(&(
            Side::Right,
            OnEof::Loop,
            Hwdec::AutoSafe,
            SubLang::ZhHant,
            VideoWheel::Volume,
            PhotoDefault::Actual,
        ))
        .unwrap();
        assert_eq!(
            json,
            r#"["right","loop","auto_safe","zh_hant","volume","actual"]"#
        );
    }

    #[test]
    fn list_entries_cannot_create_new_ids_or_settings() {
        let dir = crate::test_util::TestDir::new("settings-list");
        let file = dir.join("settings.conf");
        let settings = Settings {
            disabled_plugins: vec![
                "good.id".into(),
                "one,two".into(),
                "bad\nvolume = 17".into(),
            ],
            hidden_dock_buttons: vec![
                "loop".into(),
                "screenshot,share".into(),
                "x\r\nalways_on_top = true".into(),
            ],
            ..Settings::default()
        };
        settings.save_to(&file).unwrap();
        let loaded = Settings::load_from(&file);
        assert_eq!(loaded.disabled_plugins, ["good.id"]);
        assert_eq!(loaded.hidden_dock_buttons, ["loop"]);
        assert_eq!(loaded.volume, settings.volume);
        assert_eq!(loaded.always_on_top, settings.always_on_top);
    }
}
