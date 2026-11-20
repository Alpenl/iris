//! Folder scanning for the docked directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Photo,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
}

const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mkv", "mov", "webm", "avi", "ts", "m2ts", "flv", "wmv", "mpg", "mpeg",
];
const PHOTO_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "tif", "tiff", "bmp",
];

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VIDEO_EXT.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else if PHOTO_EXT.contains(&ext.as_str()) {
        Some(Kind::Photo)
    } else {
        None
    }
}

/// Lists playable files in `dir`, sorted by name.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|path| {
            let kind = kind_of(&path)?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            Some(Entry { path, name, kind })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}
