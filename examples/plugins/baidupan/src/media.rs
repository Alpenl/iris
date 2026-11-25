//! Portable media classification, sibling subtitle matching, path helpers, and natural sorting.

use iris_plugin_sdk::api::{SourceEntryKind, v2::enhance::SubtitleFormat};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Photo,
    Audio,
}

impl MediaType {
    pub fn to_entry_kind(self) -> SourceEntryKind {
        match self {
            Self::Video => SourceEntryKind::Video,
            Self::Photo => SourceEntryKind::Photo,
            Self::Audio => SourceEntryKind::Audio,
        }
    }
}

pub fn extension_lower(name: &str) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let (_, ext) = base.rsplit_once('.')?;
    (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

pub fn classify_file(name: &str, baidu_category: i64) -> Option<MediaType> {
    if let Some(ext) = extension_lower(name) {
        match ext.as_str() {
            "mp4" | "m4v" | "mkv" | "mov" | "webm" | "avi" | "ts" | "m2ts" | "flv" | "wmv"
            | "mpg" | "mpeg" | "rmvb" | "rm" | "3gp" | "ogv" | "vob" => {
                return Some(MediaType::Video);
            }
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "heic" | "heif" | "avif" | "tif" | "tiff"
            | "bmp" => return Some(MediaType::Photo),
            "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "alac" | "aiff" | "wma"
            | "ape" | "dsf" | "dff" => return Some(MediaType::Audio),
            "srt" | "vtt" | "ass" => return None,
            _ => {}
        }
    }
    match baidu_category {
        1 => Some(MediaType::Video),
        2 => Some(MediaType::Audio),
        3 => Some(MediaType::Photo),
        _ => None,
    }
}

pub fn subtitle_format_of(name: &str) -> Option<SubtitleFormat> {
    match extension_lower(name)?.as_str() {
        "srt" => Some(SubtitleFormat::SubRip),
        "vtt" => Some(SubtitleFormat::WebVtt),
        "ass" => Some(SubtitleFormat::Ass),
        _ => None,
    }
}

/// Infers a BCP-47 language tag from common subtitle suffixes (e.g. `.chs.srt`, `.en.ass`).
pub fn infer_subtitle_lang(sub_name: &str) -> Option<String> {
    let stem = file_stem(sub_name);
    let (_, suffix) = stem.rsplit_once('.')?;
    let lower = suffix.to_ascii_lowercase();
    match lower.as_str() {
        "zh" | "zh-cn" | "chs" | "sc" | "cn" | "gb" => Some("zh-CN".into()),
        "zh-tw" | "zh-hk" | "cht" | "tc" | "big5" => Some("zh-TW".into()),
        "en" | "eng" | "en-us" => Some("en".into()),
        "ja" | "jp" | "jpn" => Some("ja".into()),
        "ko" | "kr" | "kor" => Some("ko".into()),
        _ if lower.len() <= 16
            && lower
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-') =>
        {
            Some(lower)
        }
        _ => None,
    }
}

pub fn file_stem(name: &str) -> &str {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    }
}

/// Returns true if `sub_name` is a matching external subtitle for `video_name`
/// (exact stem match or `<video_stem>.<lang>.<ext>`).
pub fn subtitle_matches_video(video_name: &str, sub_name: &str) -> bool {
    if subtitle_format_of(sub_name).is_none() {
        return false;
    }
    let v_stem = file_stem(video_name).to_ascii_lowercase();
    let s_stem = file_stem(sub_name).to_ascii_lowercase();
    if v_stem.is_empty() {
        return false;
    }
    s_stem == v_stem
        || s_stem.strip_prefix(&v_stem).is_some_and(|rest| {
            rest.starts_with('.') || rest.starts_with('_') || rest.starts_with('-')
        })
}

pub fn normalize_dir(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".into();
    }
    let mut parts = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
        } else {
            parts.push(seg);
        }
    }
    if parts.is_empty() {
        "/".into()
    } else {
        format!("/{}", parts.join("/"))
    }
}

pub fn parent_dir(path: &str) -> Option<String> {
    let norm = normalize_dir(path);
    if norm == "/" {
        return None;
    }
    match norm.rfind('/') {
        Some(0) | None => Some("/".into()),
        Some(idx) => Some(norm[..idx].to_owned()),
    }
}

pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(path)
}

/// Natural human-friendly comparison (`ep2` before `ep10`, case-insensitive).
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
