//! Portable media classification; never opens a native path.
use std::{cmp::Ordering, path::Path};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Photo,
    Audio,
}
pub fn kind_of(path: &Path) -> Option<Kind> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" | "mkv" | "mov" | "webm" | "avi" | "ts" | "m2ts" | "flv" | "wmv"
            | "mpg" | "mpeg" => Kind::Video,
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "heic" | "heif" | "avif" | "tif" | "tiff"
            | "bmp" => Kind::Photo,
            "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "alac" | "aiff" | "wma" => {
                Kind::Audio
            }
            _ => return None,
        },
    )
}
pub fn is_subtitle(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "srt" | "vtt" | "ass"))
}
/// Compares file names the way people read them: runs of ASCII digits
/// compare by value (`ep2` before `ep10`), other characters compare
/// case-insensitively. Names that only differ in case or leading zeros
/// fall back to a plain comparison, so the order is total and stable.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ra, mut rb) = (a, b);
    loop {
        match (ra.chars().next(), rb.chars().next()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (m, rest_a) = take_digits(ra);
                let (n, rest_b) = take_digits(rb);
                ra = rest_a;
                rb = rest_b;
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
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
                ra = &ra[c.len_utf8()..];
                rb = &rb[d.len_utf8()..];
            }
        }
    }
}

fn take_digits(s: &str) -> (&str, &str) {
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    (&s[..end], &s[end..])
}
