//! Folder scanning, sorting and media classification for the docked directory.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Photo,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Name,
    Modified,
    Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Clone)]
pub struct FolderEntry {
    pub path: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub entries: Vec<Entry>,
    pub subfolders: Vec<FolderEntry>,
}

const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mkv", "mov", "webm", "avi", "ts", "m2ts", "flv", "wmv", "mpg", "mpeg",
];
const PHOTO_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "tif", "tiff", "bmp",
];
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "alac", "aiff", "wma",
];
const SUBTITLE_EXT: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "sup", "idx", "lrc"];

pub fn is_subtitle(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUBTITLE_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VIDEO_EXT.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else if PHOTO_EXT.contains(&ext.as_str()) {
        Some(Kind::Photo)
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Some(Kind::Audio)
    } else {
        None
    }
}

pub fn entry_from_path(path: PathBuf) -> Option<Entry> {
    let kind = kind_of(&path)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let meta = std::fs::metadata(&path).ok();
    let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
    let modified_secs = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some(Entry {
        path,
        name,
        kind,
        size,
        modified_secs,
    })
}

/// Scans `dir` for playable media files and immediate non-hidden subfolders,
/// sorted in natural name order.
pub fn scan_dir(dir: &Path) -> ScanResult {
    let Ok(read) = std::fs::read_dir(dir) else {
        return ScanResult::default();
    };
    let mut entries = Vec::new();
    let mut subfolders = Vec::new();
    for item in read.flatten() {
        let path = item.path();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = item.metadata() else {
            continue;
        };
        if meta.is_dir() {
            subfolders.push(FolderEntry { path, name });
        } else if meta.is_file()
            && let Some(kind) = kind_of(&path)
        {
            let size = meta.len();
            let modified_secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            entries.push(Entry {
                path,
                name,
                kind,
                size,
                modified_secs,
            });
        }
    }
    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    subfolders.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    ScanResult {
        entries,
        subfolders,
    }
}

/// Lists playable files in `dir`, in natural name order (see [`natural_cmp`]).
pub fn scan(dir: &Path) -> Vec<Entry> {
    scan_dir(dir).entries
}

pub fn sort_entries(entries: &mut [Entry], by: SortBy, order: SortOrder) {
    entries.sort_by(|a, b| {
        let ord = match by {
            SortBy::Name => natural_cmp(&a.name, &b.name),
            SortBy::Modified => a
                .modified_secs
                .cmp(&b.modified_secs)
                .then_with(|| natural_cmp(&a.name, &b.name)),
            SortBy::Size => a
                .size
                .cmp(&b.size)
                .then_with(|| natural_cmp(&a.name, &b.name)),
        };
        match order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    });
}

pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    const MB: f64 = KB * 1024.;
    const GB: f64 = MB * 1024.;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Compares file names the way people read them: runs of ASCII digits
/// compare by value (`ep2` before `ep10`), other characters compare
/// case-insensitively. Names that only differ in case or leading zeros
/// fall back to a plain comparison, so the order is total and stable.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let m = digit_run(&mut x);
                let n = digit_run(&mut y);
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                // Without leading zeros, a longer run is a bigger number.
                let ord = m.len().cmp(&n.len()).then_with(|| m.cmp(n));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                let ord = c.to_lowercase().cmp(d.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                x.next();
                y.next();
            }
        }
    }
}

fn digit_run(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(c) = chars.next_if(char::is_ascii_digit) {
        run.push(c);
    }
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn numbers_compare_by_value() {
        assert_eq!(
            sorted(&["ep10.mkv", "ep2.mkv", "ep1.mkv", "ep100.mkv"]),
            ["ep1.mkv", "ep2.mkv", "ep10.mkv", "ep100.mkv"]
        );
        assert_eq!(
            sorted(&["IMG_0010.HEIC", "IMG_9.heic", "img_0002.jpg"]),
            ["img_0002.jpg", "IMG_9.heic", "IMG_0010.HEIC"]
        );
    }

    #[test]
    fn letters_ignore_case() {
        assert_eq!(natural_cmp("b.png", "A.png"), Greater);
        assert_eq!(natural_cmp("abc", "ABD"), Less);
    }

    #[test]
    fn ties_are_broken_deterministically() {
        assert_ne!(natural_cmp("a.jpg", "A.jpg"), Equal);
        assert_ne!(natural_cmp("x01", "x1"), Equal);
        assert_eq!(natural_cmp("same", "same"), Equal);
        assert_eq!(natural_cmp("ep", "ep1"), Less);
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert_eq!(
            natural_cmp("a99999999999999999999999", "a100000000000000000000000"),
            Less
        );
    }

    #[test]
    fn classifies_audio_and_subtitles() {
        assert_eq!(kind_of(Path::new("track.flac")), Some(Kind::Audio));
        assert_eq!(kind_of(Path::new("clip.mkv")), Some(Kind::Video));
        assert_eq!(kind_of(Path::new("shot.HEIC")), Some(Kind::Photo));
        assert!(is_subtitle(Path::new("movie.chs.ass")));
        assert!(!is_subtitle(Path::new("movie.mp4")));
    }

    #[test]
    fn sorts_by_size_and_modified() {
        let mut items = vec![
            Entry {
                path: PathBuf::from("b.mp4"),
                name: "b.mp4".into(),
                kind: Kind::Video,
                size: 100,
                modified_secs: 20,
            },
            Entry {
                path: PathBuf::from("a.mp4"),
                name: "a.mp4".into(),
                kind: Kind::Video,
                size: 500,
                modified_secs: 10,
            },
        ];
        sort_entries(&mut items, SortBy::Size, SortOrder::Desc);
        assert_eq!(items[0].name, "a.mp4");
        sort_entries(&mut items, SortBy::Modified, SortOrder::Desc);
        assert_eq!(items[0].name, "b.mp4");
    }
}
