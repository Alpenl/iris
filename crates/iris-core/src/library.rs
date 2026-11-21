//! Folder scanning for the docked directory.

use std::cmp::Ordering;
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
    } else {
        None
    }
}

/// Lists playable files in `dir`, in natural name order (see [`natural_cmp`]).
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
    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    entries
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
    use super::natural_cmp;
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
}
