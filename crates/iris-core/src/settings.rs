//! User settings, kept in a small `key = value` file:
//! macOS `~/Library/Application Support/Iris/settings.conf`,
//! Windows `%APPDATA%\Iris\settings.conf`. `IRIS_SETTINGS` overrides the path.

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Side {
    #[default]
    Left,
    Right,
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Which edge the directory sits on, docked and in the fullscreen popup.
    pub directory_side: Side,
    pub volume: Option<f64>,
    pub speed: Option<f64>,
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
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("directory_side", "left") => s.directory_side = Side::Left,
                ("directory_side", "right") => s.directory_side = Side::Right,
                ("volume", v) => {
                    s.volume = v
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && (0. ..=100.).contains(v))
                }
                ("speed", v) => {
                    s.speed = v
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && (0.1..=8.).contains(v))
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let side = match self.directory_side {
            Side::Left => "left",
            Side::Right => "right",
        };
        let result = p
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let mut text = format!("directory_side = {side}\n");
                if let Some(v) = self.volume {
                    text.push_str(&format!("volume = {v}\n"));
                }
                if let Some(v) = self.speed {
                    text.push_str(&format!("speed = {v}\n"));
                }
                std::fs::write(&p, text)
            });
        if let Err(e) = result {
            log::error!("saving settings to {} failed: {e}", p.display());
        }
    }
}
