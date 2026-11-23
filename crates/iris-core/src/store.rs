//! Unified URI-keyed personal records and recent media list (`records.tsv`, `recent.tsv`).
//! Stored in the application data directory, never beside media files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::fs_util;
use crate::media_uri::MediaUri;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record {
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
}

const MAX_RECENT: usize = 20;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Default, Debug)]
pub struct Store {
    records: BTreeMap<String, Record>,
    recent: Vec<MediaUri>,
    dirty: bool,
    backing_dir: Option<PathBuf>,
}

impl Store {
    pub fn load() -> Self {
        match fs_util::data_dir() {
            Some(dir) => Self::load_from_dir(&dir),
            None => Self::default(),
        }
    }

    pub fn load_from_dir(dir: &Path) -> Self {
        let mut records = BTreeMap::new();
        // Load legacy `uri_records.tsv` first so newer `records.tsv` entries win on collision.
        Self::read_records_into(&dir.join("uri_records.tsv"), &mut records);
        Self::read_records_into(&dir.join("records.tsv"), &mut records);

        let mut recent = Vec::new();
        Self::read_recent_into(&dir.join("recent.tsv"), &mut recent);
        Self::read_recent_into(&dir.join("recent_uris.tsv"), &mut recent);

        Self {
            records,
            recent,
            dirty: false,
            backing_dir: Some(dir.to_path_buf()),
        }
    }

    fn target_dir(&self) -> Option<PathBuf> {
        self.backing_dir.clone().or_else(fs_util::data_dir)
    }

    pub fn records(&self) -> &BTreeMap<String, Record> {
        &self.records
    }

    pub fn recent(&self) -> &[MediaUri] {
        &self.recent
    }

    pub fn favorites(&self) -> Vec<MediaUri> {
        self.records
            .iter()
            .filter(|(_, r)| r.favorite)
            .map(|(k, _)| MediaUri::parse(k))
            .collect()
    }

    fn read_recent_into(path: &Path, out: &mut Vec<MediaUri>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            if out.len() >= MAX_RECENT {
                break;
            }
            if let Some(uri) = decode_any_media_uri(line.trim())
                && !out.contains(&uri)
            {
                out.push(uri);
            }
        }
    }

    fn read_records_into(path: &Path, out: &mut BTreeMap<String, Record>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 4 {
                continue;
            }
            let (Some(uri), Ok(position), Ok(duration)) = (
                decode_any_media_uri(fields[0]),
                fields[2].parse::<f64>(),
                fields[3].parse::<f64>(),
            ) else {
                continue;
            };
            if !position.is_finite() || !duration.is_finite() || position < 0. || duration < 0. {
                continue;
            }
            out.insert(
                uri.to_uri_string(),
                Record {
                    favorite: fields[1] == "1",
                    position,
                    duration,
                },
            );
        }
    }

    fn save_recent(&self) {
        let Some(dir) = self.target_dir() else { return };
        let mut text = String::with_capacity(self.recent.len() * 64);
        for u in self.recent.iter().take(MAX_RECENT) {
            push_hex(u.to_uri_string().as_bytes(), &mut text);
            text.push('\n');
        }
        if let Err(e) = fs_util::atomic_write(&dir.join("recent.tsv"), text.as_bytes()) {
            log::error!("saving recent list failed: {e}");
        }
    }

    pub fn save(&mut self) {
        let Some(dir) = self.target_dir() else { return };
        let text = self.format_records();
        if let Err(e) = fs_util::atomic_write(&dir.join("records.tsv"), text.as_bytes()) {
            log::error!("saving records failed: {e}");
        } else {
            self.dirty = false;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Captures serialized records in memory for off-UI-thread atomic saving,
    /// performing zero synchronous disk I/O on the calling thread.
    pub fn take_dirty_snapshot(&mut self) -> Option<(PathBuf, String)> {
        if !self.dirty {
            return None;
        }
        let dir = self.target_dir()?;
        self.dirty = false;
        Some((dir.join("records.tsv"), self.format_records()))
    }

    /// Writes a previously captured snapshot to disk (safe to run on a worker thread).
    pub fn write_snapshot(path: &Path, text: &str) {
        if let Err(e) = fs_util::atomic_write(path, text.as_bytes()) {
            log::error!("saving records failed: {e}");
        }
    }

    fn format_records(&self) -> String {
        let mut text = String::with_capacity(self.records.len() * 96);
        for (k, r) in &self.records {
            push_hex(k.as_bytes(), &mut text);
            let _ = writeln!(
                text,
                "\t{}\t{:.3}\t{:.3}",
                u8::from(r.favorite),
                r.position,
                r.duration
            );
        }
        text
    }

    pub fn push_recent(&mut self, uri: &MediaUri) {
        self.recent.retain(|x| x != uri);
        self.recent.insert(0, uri.clone());
        self.recent.truncate(MAX_RECENT);
        self.save_recent();
    }

    pub fn push_recent_path(&mut self, path: &Path) {
        self.push_recent(&MediaUri::Local(path.to_path_buf()));
    }

    pub fn clear_recent(&mut self) {
        if !self.recent.is_empty() {
            self.recent.clear();
            self.save_recent();
        }
    }

    pub fn is_favorite(&self, uri: &MediaUri) -> bool {
        self.records
            .get(&uri.to_uri_string())
            .is_some_and(|r| r.favorite)
    }

    pub fn is_favorite_path(&self, path: &Path) -> bool {
        self.is_favorite(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn toggle_favorite(&mut self, uri: &MediaUri) -> bool {
        let key = uri.to_uri_string();
        let r = self.records.entry(key.clone()).or_default();
        r.favorite = !r.favorite;
        let fav = r.favorite;
        if !fav && r.position <= 0. && r.duration <= 0. {
            self.records.remove(&key);
        }
        self.save();
        fav
    }

    pub fn toggle_favorite_path(&mut self, path: &Path) -> bool {
        self.toggle_favorite(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn remove_favorite(&mut self, uri: &MediaUri) -> bool {
        let key = uri.to_uri_string();
        if let Some(r) = self.records.get_mut(&key)
            && r.favorite
        {
            r.favorite = false;
            if r.position <= 0. && r.duration <= 0. {
                self.records.remove(&key);
            }
            self.save();
            return true;
        }
        false
    }

    pub fn remove_favorite_path(&mut self, path: &Path) -> bool {
        self.remove_favorite(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn progress_ratio(&self, uri: &MediaUri) -> Option<f32> {
        let r = self.records.get(&uri.to_uri_string())?;
        if r.duration > 0. && r.position >= 5. && r.position < r.duration - 5. {
            Some((r.position / r.duration).clamp(0., 1.) as f32)
        } else {
            None
        }
    }

    pub fn progress_ratio_uri(&self, uri: &MediaUri) -> Option<f32> {
        self.progress_ratio(uri)
    }

    pub fn progress_ratio_path(&self, path: &Path) -> Option<f32> {
        self.progress_ratio(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn resume(&self, uri: &MediaUri) -> f64 {
        self.records
            .get(&uri.to_uri_string())
            .filter(|r| r.position >= 5. && r.position < r.duration - 5.)
            .map_or(0., |r| r.position)
    }

    pub fn resume_path(&self, path: &Path) -> f64 {
        self.resume(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn remember(&mut self, uri: &MediaUri, position: f64, duration: f64, eof: bool) -> bool {
        if !position.is_finite() || !duration.is_finite() || duration <= 0. {
            return false;
        }
        let key = uri.to_uri_string();
        if key.is_empty() {
            return false;
        }
        if eof {
            let changed = match self.records.get_mut(&key) {
                Some(r) if r.favorite => {
                    let diff = r.position > 0. || (r.duration - duration).abs() > 0.01;
                    r.position = 0.;
                    r.duration = duration;
                    diff
                }
                Some(_) => {
                    self.records.remove(&key);
                    true
                }
                None => false,
            };
            if changed {
                self.dirty = true;
            }
            return changed;
        }
        let next_pos = position.max(0.);
        if let Some(r) = self.records.get_mut(&key) {
            if (r.position - next_pos).abs() < 0.25 && (r.duration - duration).abs() < 0.01 {
                return false;
            }
            r.position = next_pos;
            r.duration = duration;
            self.dirty = true;
            return true;
        }
        if next_pos < 5. || next_pos >= duration - 5. {
            return false;
        }
        self.records.insert(
            key,
            Record {
                favorite: false,
                position: next_pos,
                duration,
            },
        );
        self.dirty = true;
        true
    }

    pub fn remember_path(&mut self, path: &Path, position: f64, duration: f64, eof: bool) -> bool {
        self.remember(
            &MediaUri::Local(path.to_path_buf()),
            position,
            duration,
            eof,
        )
    }

    pub fn reset_progress(&mut self, uri: &MediaUri) -> bool {
        let key = uri.to_uri_string();
        let Some(r) = self.records.get_mut(&key) else {
            return false;
        };
        if r.position <= 0. && r.duration <= 0. {
            return false;
        }
        r.position = 0.;
        r.duration = 0.;
        if !r.favorite {
            self.records.remove(&key);
        }
        self.save();
        true
    }

    pub fn reset_progress_path(&mut self, path: &Path) -> bool {
        self.reset_progress(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn clear_progress(&mut self) -> usize {
        let mut cleared = 0;
        self.records.retain(|_, r| {
            if r.position > 0. || r.duration > 0. {
                cleared += 1;
                r.position = 0.;
                r.duration = 0.;
            }
            r.favorite
        });
        if cleared > 0 {
            self.save();
        }
        cleared
    }
}

fn push_hex(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
}

/// Decodes a hex-encoded canonical URI, legacy UTF-8 path/WebDAV URI, or legacy OS path.
fn decode_any_media_uri(hex_text: &str) -> Option<MediaUri> {
    if hex_text.is_empty()
        || !hex_text.len().is_multiple_of(2)
        || hex_text.len() > 262144
        || !hex_text.is_ascii()
    {
        return None;
    }
    let bytes: Vec<u8> = (0..hex_text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_text[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    if !bytes.contains(&0)
        && let Ok(s) = std::str::from_utf8(&bytes)
        && !s.is_empty()
    {
        return Some(MediaUri::parse(s));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(MediaUri::Local(std::ffi::OsString::from_vec(bytes).into()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        Some(MediaUri::Local(
            std::ffi::OsString::from_wide(&units).into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_str(s: &str) -> String {
        let mut out = String::new();
        push_hex(s.as_bytes(), &mut out);
        out
    }

    #[test]
    fn unified_store_roundtrips_and_migrates_legacy_path_and_webdav_tsvs() {
        let dir = std::env::temp_dir().join(format!("iris-store-unified-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Seed legacy `records.tsv` (raw local path) + legacy `uri_records.tsv` (`webdav://` URI)
        // + legacy `recent.tsv` + `recent_uris.tsv` + corrupt row.
        let legacy_local = "/素材/hello\tworld\n100%.mp4";
        let legacy_webdav = "webdav://home-nas/Movies/Sci%20Fi/ep1.mkv";
        std::fs::write(
            dir.join("records.tsv"),
            format!(
                "{}\t1\t25.000\t100.000\ninvalid\t1\tNaN\t60\n",
                hex_str(legacy_local)
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("uri_records.tsv"),
            format!("{}\t1\t30.000\t120.000\n", hex_str(legacy_webdav)),
        )
        .unwrap();
        std::fs::write(
            dir.join("recent.tsv"),
            format!("{}\n", hex_str(legacy_local)),
        )
        .unwrap();
        std::fs::write(
            dir.join("recent_uris.tsv"),
            format!("{}\n", hex_str(legacy_webdav)),
        )
        .unwrap();

        let mut store = Store::load_from_dir(&dir);
        assert!(store.is_favorite_path(Path::new(legacy_local)));
        assert_eq!(store.resume_path(Path::new(legacy_local)), 25.0);

        let upgraded_webdav = MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: "home-nas".into(),
            entry_id: "/Movies/Sci Fi/ep1.mkv".into(),
        };
        assert!(store.is_favorite(&upgraded_webdav));
        assert_eq!(store.progress_ratio_uri(&upgraded_webdav), Some(0.25));
        assert_eq!(store.recent().len(), 2);

        // Remember new progress and flush purely via snapshot (no main-thread I/O).
        let cloud_item = MediaUri::Plugin {
            plugin_id: "acme.cloud".into(),
            source_id: "drive".into(),
            entry_id: "ep2".into(),
        };
        assert!(store.remember(&cloud_item, 15.0, 60.0, false));
        assert!(store.is_dirty());
        let (snap_path, snap_text) = store.take_dirty_snapshot().unwrap();
        assert!(!store.is_dirty());
        Store::write_snapshot(&snap_path, &snap_text);

        // Remove legacy uri_records.tsv to prove single `records.tsv` holds everything now.
        let _ = std::fs::remove_file(dir.join("uri_records.tsv"));
        let mut reloaded = Store::load_from_dir(&dir);
        assert!(reloaded.is_favorite(&upgraded_webdav));
        assert_eq!(reloaded.resume(&cloud_item), 15.0);

        // EOF prunes non-favorite progress while keeping favorites.
        assert!(reloaded.remember(&cloud_item, 60.0, 60.0, true));
        assert_eq!(reloaded.clear_progress(), 2);
        assert!(reloaded.is_favorite(&upgraded_webdav));
        assert_eq!(reloaded.resume(&upgraded_webdav), 0.0);
        assert_eq!(reloaded.resume(&cloud_item), 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
