//! Folder scanning, slow-share-safe path resolution, sorting, and media classification.

use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Threshold above which slow network shares skip per-file `stat`/`metadata()` syscalls.
const SLOW_SHARE_FAST_SCAN_THRESHOLD: usize = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        .is_some_and(|e| SUBTITLE_EXT.iter().any(|k| e.eq_ignore_ascii_case(k)))
}

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?;
    if VIDEO_EXT.iter().any(|k| ext.eq_ignore_ascii_case(k)) {
        Some(Kind::Video)
    } else if PHOTO_EXT.iter().any(|k| ext.eq_ignore_ascii_case(k)) {
        Some(Kind::Photo)
    } else if AUDIO_EXT.iter().any(|k| ext.eq_ignore_ascii_case(k)) {
        Some(Kind::Audio)
    } else {
        None
    }
}

fn meta_size_and_mtime(meta: Option<std::fs::Metadata>) -> (u64, u64) {
    meta.map_or((0, 0), |m| {
        let secs = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        (m.len(), secs)
    })
}

pub fn entry_from_path(path: PathBuf) -> Option<Entry> {
    let kind = kind_of(&path)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let (size, modified_secs) = if is_slow_network_path(&path) {
        (0, 0)
    } else {
        meta_size_and_mtime(std::fs::metadata(&path).ok())
    };
    Some(Entry {
        path,
        name,
        kind,
        size,
        modified_secs,
    })
}

fn is_unc_str(raw: &str) -> bool {
    if raw.len() >= 8 && raw.as_bytes()[..8].eq_ignore_ascii_case(br"\\?\UNC\") {
        return true;
    }
    if raw.starts_with(r"\\") && !raw.starts_with(r"\\?\") && !raw.starts_with(r"\\.\") {
        let rest = &raw[2..];
        return rest.contains('\\') && !rest.starts_with('\\');
    }
    false
}

/// Detects paths on potentially high-latency network mounts or external volumes:
/// - Windows UNC paths (`\\server\share\...` and `\\?\UNC\server\share\...`)
/// - macOS `/Volumes/<mount>/...` non-root mounts (excluding the boot volume symlink to `/`)
pub fn is_slow_network_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if let Some(Component::Prefix(prefix)) = path.components().next()
            && matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        {
            return true;
        }
    }

    let raw = path.to_string_lossy();
    if is_unc_str(&raw) {
        return true;
    }

    if let Some(after_volumes) = raw.strip_prefix("/Volumes/") {
        let mount_name = after_volumes.split('/').next().unwrap_or("");
        if !mount_name.is_empty() && mount_name != "." && mount_name != ".." {
            let mount_root = Path::new("/Volumes").join(mount_name);
            if let Ok(link_target) = std::fs::read_link(&mount_root)
                && link_target == Path::new("/")
            {
                return false;
            }
            return true;
        }
    }

    false
}

fn normalize_unc_str(raw: &str) -> PathBuf {
    let (prefix, tail) = if raw.len() >= 8 && raw.as_bytes()[..8].eq_ignore_ascii_case(br"\\?\UNC\")
    {
        (r"\\?\UNC\", &raw[8..])
    } else {
        (r"\\", &raw[2..])
    };

    let mut parts = tail.split(['\\', '/']).filter(|s| !s.is_empty());
    let Some(server) = parts.next() else {
        return PathBuf::from(raw);
    };
    let Some(share) = parts.next() else {
        return PathBuf::from(format!("{prefix}{server}"));
    };

    let mut stack: Vec<&str> = Vec::new();
    for seg in parts {
        if seg == "." {
            continue;
        }
        if seg == ".." {
            stack.pop();
            continue;
        }
        stack.push(seg);
    }

    let mut out = format!(r"{prefix}{server}\{share}");
    for seg in stack {
        out.push('\\');
        out.push_str(seg);
    }
    PathBuf::from(out)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if is_unc_str(&raw) {
        return normalize_unc_str(&raw);
    }

    let base = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };

    let mut normalized = PathBuf::new();
    let mut root_locked = false;
    for comp in base.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => {
                normalized.push(comp.as_os_str());
                root_locked = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|c| matches!(c, Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else if !root_locked {
                    normalized.push("..");
                }
            }
            Component::Normal(seg) => normalized.push(seg),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Resolves `path` safely across local disks and network shares.
/// Avoids blocking `std::fs::canonicalize()` syscalls (and Windows `\\?\UNC\` rewrites)
/// on slow network shares, falling back gracefully to lexical normalization.
pub fn safe_resolve_path(path: &Path) -> PathBuf {
    if is_slow_network_path(path) {
        return lexical_normalize(path);
    }
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

fn scan_dir_with_mode(dir: &Path, slow_share: bool) -> ScanResult {
    let Ok(read) = std::fs::read_dir(dir) else {
        return ScanResult::default();
    };
    let items: Vec<std::fs::DirEntry> = read
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();

    let fast_skip_stat = slow_share && items.len() > SLOW_SHARE_FAST_SCAN_THRESHOLD;
    let mut entries = Vec::new();
    let mut subfolders = Vec::new();

    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        let Ok(ft) = item.file_type() else {
            continue;
        };
        if ft.is_dir() {
            subfolders.push(FolderEntry { path, name });
        } else if ft.is_file() {
            let Some(kind) = kind_of(&path) else {
                continue;
            };
            let (size, modified_secs) = if fast_skip_stat {
                (0, 0)
            } else {
                meta_size_and_mtime(item.metadata().ok())
            };
            entries.push(Entry {
                path,
                name,
                kind,
                size,
                modified_secs,
            });
        } else if ft.is_symlink() {
            if fast_skip_stat {
                if let Some(kind) = kind_of(&path) {
                    entries.push(Entry {
                        path,
                        name,
                        kind,
                        size: 0,
                        modified_secs: 0,
                    });
                }
            } else if let Ok(meta) = item.metadata() {
                if meta.is_dir() {
                    subfolders.push(FolderEntry { path, name });
                } else if meta.is_file()
                    && let Some(kind) = kind_of(&path)
                {
                    let (size, modified_secs) = meta_size_and_mtime(Some(meta));
                    entries.push(Entry {
                        path,
                        name,
                        kind,
                        size,
                        modified_secs,
                    });
                }
            }
        }
    }

    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    subfolders.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    ScanResult {
        entries,
        subfolders,
    }
}

/// Scans `dir` for playable media files and immediate non-hidden subfolders,
/// sorted in natural name order.
pub fn scan_dir(dir: &Path) -> ScanResult {
    scan_dir_with_mode(dir, false)
}

/// Scans `dir` for media files and subfolders, switching automatically to a zero-`stat`
/// fast scan on slow network shares with more than 150 entries.
pub fn scan_local_or_share(dir: &Path) -> ScanResult {
    scan_dir_with_mode(dir, is_slow_network_path(dir))
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

    #[test]
    fn slow_network_path_helpers_and_large_share_fast_scan() {
        assert!(is_slow_network_path(Path::new(r"\\nas\media\Movies")));
        assert!(is_slow_network_path(Path::new(r"\\?\UNC\nas\media\Movies")));
        assert!(!is_slow_network_path(Path::new("/Users/alpen/Movies")));

        let resolved_unc = safe_resolve_path(Path::new(r"\\nas\media\Movies\..\Anime\.\ep1.mkv"));
        assert_eq!(resolved_unc, PathBuf::from(r"\\nas\media\Anime\ep1.mkv"));

        let dir = std::env::temp_dir().join(format!("iris-fast-share-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("SubFolder")).unwrap();
        for i in 1..=160 {
            std::fs::write(dir.join(format!("ep{i}.mp4")), b"stub").unwrap();
        }
        let res = scan_dir_with_mode(&dir, true);
        assert_eq!(res.subfolders.len(), 1);
        assert_eq!(res.entries.len(), 160);
        assert_eq!(res.entries[0].name, "ep1.mp4");
        assert_eq!(res.entries[1].name, "ep2.mp4");
        assert_eq!(res.entries[9].name, "ep10.mp4");
        assert_eq!(res.entries[0].size, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
