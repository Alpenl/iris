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
}

fn path() -> Option<PathBuf> {
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
            .and_then(|()| std::fs::write(&p, format!("directory_side = {side}\n")));
        if let Err(e) = result {
            eprintln!("[iris] saving settings to {} failed: {e}", p.display());
        }
    }
}
