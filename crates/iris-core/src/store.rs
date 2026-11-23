//! Unified URI-keyed personal records and recent media list (`records.tsv`, `recent.tsv`).
//! Stored in the application data directory, never beside media files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
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

type Records = BTreeMap<String, Record>;

#[derive(Debug, Default)]
struct RecordEdit {
    favorite: Option<bool>,
    progress_before: Option<(f64, f64)>,
    progress: Option<(f64, f64)>,
}

impl RecordEdit {
    fn remember_progress(&mut self, before: (f64, f64), after: (f64, f64)) {
        if self.progress.is_none() {
            self.progress_before = Some(before);
        }
        self.progress = Some(after);
    }

    fn matches_progress(&self, record: Record) -> bool {
        // TSV rounds to milliseconds. A conflicting on-disk edit, including a
        // reset by another window while this save was pending, takes precedence.
        self.progress_before.is_none_or(|(pos, dur)| {
            (record.position - pos).abs() <= 0.000_501 && (record.duration - dur).abs() <= 0.000_501
        })
    }
}

#[derive(Default, Debug)]
pub struct Store {
    records: BTreeMap<String, Record>,
    recent: Vec<MediaUri>,
    edits: BTreeMap<String, RecordEdit>,
    recent_pushes: Vec<MediaUri>,
    clear_all_progress: bool,
    clear_all_recent: bool,
    refresh_records: bool,
    refresh_recent: bool,
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
        let mut store = Self {
            backing_dir: Some(dir.to_path_buf()),
            refresh_records: true,
            refresh_recent: true,
            ..Self::default()
        };
        store.save();
        store
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

    fn read_recent_into(text: &str, out: &mut Vec<MediaUri>) -> bool {
        let mut scrub = false;
        for line in text.lines() {
            let Some(uri) = decode_any_media_uri(line.trim()) else {
                scrub = true;
                continue;
            };
            if !uri.may_share_identity() {
                scrub = true;
            } else if out.len() < MAX_RECENT && !out.contains(&uri) {
                out.push(uri);
            }
        }
        scrub
    }

    fn read_records_into(text: &str, out: &mut Records) -> bool {
        let mut scrub = false;
        for line in text.lines() {
            let mut fields = line.split('\t');
            let (Some(raw), Some(favorite), Some(position), Some(duration), None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                scrub = true;
                continue;
            };
            let (Some(uri), Ok(position), Ok(duration)) = (
                decode_any_media_uri(raw),
                position.parse::<f64>(),
                duration.parse::<f64>(),
            ) else {
                scrub = true;
                continue;
            };
            if !uri.may_share_identity()
                || !position.is_finite()
                || !duration.is_finite()
                || position < 0.
                || duration < 0.
            {
                scrub = true;
                continue;
            }
            out.insert(
                uri.to_uri_string(),
                Record {
                    favorite: favorite == "1",
                    position,
                    duration,
                },
            );
        }
        scrub
    }

    fn write_recent(path: &Path, recent: &[MediaUri]) -> io::Result<()> {
        let mut text = String::with_capacity(recent.len() * 64);
        for u in recent {
            push_hex(u.to_uri_string().as_bytes(), &mut text);
            text.push('\n');
        }
        fs_util::atomic_write(path, text)
    }

    pub fn save(&mut self) {
        if !self.is_dirty() {
            return;
        }
        let Some(dir) = self.target_dir() else { return };
        let _lock = match fs_util::lock_store(&dir) {
            Ok(lock) => lock,
            Err(e) => {
                log::error!("locking media store failed: {e}");
                return;
            }
        };
        if (self.refresh_records || self.clear_all_progress || !self.edits.is_empty())
            && let Err(e) = self.sync_records(&dir)
        {
            log::error!("synchronizing records failed: {e}");
        }
        if (self.refresh_recent || self.clear_all_recent || !self.recent_pushes.is_empty())
            && let Err(e) = self.sync_recent(&dir)
        {
            log::error!("synchronizing recent list failed: {e}");
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.refresh_records
            || self.refresh_recent
            || self.clear_all_progress
            || self.clear_all_recent
            || !self.edits.is_empty()
            || !self.recent_pushes.is_empty()
    }

    fn sync_records(&mut self, dir: &Path) -> io::Result<()> {
        let path = dir.join("records.tsv");
        let legacy_path = dir.join("uri_records.tsv");
        let mut records = Records::new();
        let legacy = read_optional(&legacy_path)?;
        if let Some(text) = &legacy {
            Self::read_records_into(text, &mut records);
        }
        let scrub =
            read_optional(&path)?.is_some_and(|text| Self::read_records_into(&text, &mut records));
        if legacy.is_some() {
            // Commit the merged base and retire the old file before accepting deletes.
            // Repeating this pair after interruption is idempotent.
            Self::write_records(&path, &records)?;
            std::fs::remove_file(legacy_path)?;
        }
        let mut changed = scrub && legacy.is_none();
        if self.clear_all_progress {
            changed |= clear_record_progress(&mut records) > 0;
        }
        for (key, edit) in &self.edits {
            let old = records.get(key).copied();
            let mut record = old.unwrap_or_default();
            if let Some(favorite) = edit.favorite {
                record.favorite = favorite;
            }
            if let Some((position, duration)) = edit.progress
                && edit.matches_progress(record)
            {
                record.position = position;
                record.duration = duration;
            }
            if record == Record::default() {
                changed |= records.remove(key).is_some();
            } else if old != Some(record) {
                records.insert(key.clone(), record);
                changed = true;
            }
        }
        if changed {
            Self::write_records(&path, &records)?;
        }
        self.records = records;
        self.edits.clear();
        self.clear_all_progress = false;
        self.refresh_records = false;
        Ok(())
    }

    fn sync_recent(&mut self, dir: &Path) -> io::Result<()> {
        let path = dir.join("recent.tsv");
        let legacy_path = dir.join("recent_uris.tsv");
        let mut recent = Vec::new();
        let scrub =
            read_optional(&path)?.is_some_and(|text| Self::read_recent_into(&text, &mut recent));
        let legacy = read_optional(&legacy_path)?;
        if let Some(text) = &legacy {
            Self::read_recent_into(text, &mut recent);
            Self::write_recent(&path, &recent)?;
            std::fs::remove_file(legacy_path)?;
        }
        let mut changed = scrub && legacy.is_none();
        if self.clear_all_recent && !recent.is_empty() {
            recent.clear();
            changed = true;
        }
        for uri in self.recent_pushes.iter().rev() {
            changed |= promote_recent(&mut recent, uri);
        }
        if changed {
            Self::write_recent(&path, &recent)?;
        }
        self.recent = recent;
        self.recent_pushes.clear();
        self.clear_all_recent = false;
        self.refresh_recent = false;
        Ok(())
    }

    fn write_records(path: &Path, records: &Records) -> io::Result<()> {
        let mut text = String::with_capacity(records.len() * 96);
        for (k, r) in records {
            push_hex(k.as_bytes(), &mut text);
            let _ = writeln!(
                text,
                "\t{}\t{:.3}\t{:.3}",
                u8::from(r.favorite),
                r.position,
                r.duration
            );
        }
        fs_util::atomic_write(path, text)
    }

    pub fn push_recent(&mut self, uri: &MediaUri) {
        if uri.may_share_identity() {
            promote_recent(&mut self.recent, uri);
            promote_recent(&mut self.recent_pushes, uri);
            self.save();
        }
    }

    pub fn push_recent_path(&mut self, path: &Path) {
        self.push_recent(&MediaUri::Local(path.to_path_buf()));
    }

    pub fn clear_recent(&mut self) {
        self.recent.clear();
        self.recent_pushes.clear();
        self.clear_all_recent = true;
        self.save();
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
        if !uri.may_share_identity() {
            return false;
        }
        let key = uri.to_uri_string();
        let r = self.records.entry(key.clone()).or_default();
        r.favorite = !r.favorite;
        let fav = r.favorite;
        if !fav && r.position <= 0. && r.duration <= 0. {
            self.records.remove(&key);
        }
        self.edits.entry(key).or_default().favorite = Some(fav);
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
            self.edits.entry(key).or_default().favorite = Some(false);
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
        if !uri.may_share_identity()
            || !position.is_finite()
            || !duration.is_finite()
            || duration <= 0.
        {
            return false;
        }
        let key = uri.to_uri_string();
        if key.is_empty() {
            return false;
        }
        let previous = self.records.get(&key).copied();
        let before = previous.map_or((0., 0.), |r| (r.position, r.duration));
        if eof {
            let Some(r) = previous else { return false };
            if r.favorite && r.position == 0. && (r.duration - duration).abs() <= 0.01 {
                return false;
            }
            let remaining_duration = if r.favorite { duration } else { 0. };
            self.edits
                .entry(key.clone())
                .or_default()
                .remember_progress(before, (0., remaining_duration));
            if r.favorite {
                self.records.insert(
                    key,
                    Record {
                        position: 0.,
                        duration,
                        ..r
                    },
                );
            } else {
                self.records.remove(&key);
            }
            return true;
        }
        let next_pos = position.max(0.);
        if let Some(r) = previous {
            if (r.position - next_pos).abs() < 0.25 && (r.duration - duration).abs() < 0.01 {
                return false;
            }
        } else if next_pos < 5. || next_pos >= duration - 5. {
            return false;
        }
        self.edits
            .entry(key.clone())
            .or_default()
            .remember_progress(before, (next_pos, duration));
        self.records.insert(
            key,
            Record {
                favorite: previous.is_some_and(|r| r.favorite),
                position: next_pos,
                duration,
            },
        );
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
        let edit = self.edits.entry(key).or_default();
        edit.progress = Some((0., 0.));
        edit.progress_before = None;
        self.save();
        true
    }

    pub fn reset_progress_path(&mut self, path: &Path) -> bool {
        self.reset_progress(&MediaUri::Local(path.to_path_buf()))
    }

    pub fn clear_progress(&mut self) -> usize {
        let cleared = clear_record_progress(&mut self.records);
        self.edits.retain(|_, edit| {
            edit.progress = None;
            edit.progress_before = None;
            edit.favorite.is_some()
        });
        self.clear_all_progress = true;
        self.save();
        cleared
    }
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn promote_recent(recent: &mut Vec<MediaUri>, uri: &MediaUri) -> bool {
    if recent.first() == Some(uri) {
        return false;
    }
    recent.retain(|existing| existing != uri);
    recent.insert(0, uri.clone());
    recent.truncate(MAX_RECENT);
    true
}

fn clear_record_progress(records: &mut Records) -> usize {
    let mut cleared = 0;
    records.retain(|_, r| {
        if r.position > 0. || r.duration > 0. {
            cleared += 1;
            r.position = 0.;
            r.duration = 0.;
        }
        r.favorite
    });
    cleared
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
    // Legacy Windows paths used UTF-16LE, including in data directories subsequently
    // moved to Unix. Decode textual URIs before treating any bytes as an opaque OS path.
    #[cfg(unix)]
    if !bytes.contains(&0) {
        use std::os::unix::ffi::OsStringExt;
        return Some(MediaUri::Local(std::ffi::OsString::from_vec(bytes).into()));
    }
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    if units.contains(&0) {
        return None;
    }
    if let Ok(text) = String::from_utf16(&units) {
        return Some(MediaUri::parse(&text));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        Some(MediaUri::Local(
            std::ffi::OsString::from_wide(&units).into(),
        ))
    }
    #[cfg(unix)]
    {
        None
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

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iris-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scrub_current_files_even_past_the_recent_limit() {
        let dir = test_dir("wide-scrub");
        let wide = "https://cdn.test/video?ticket=secret"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut key = String::new();
        push_hex(&wide, &mut key);
        let mut recent = String::new();
        for i in 0..MAX_RECENT {
            push_hex(format!("file-{i}.mkv").as_bytes(), &mut recent);
            recent.push('\n');
        }
        recent.push_str(&key);
        std::fs::write(dir.join("recent.tsv"), recent).unwrap();
        std::fs::write(dir.join("records.tsv"), format!("{key}\t1\t10\t20\n")).unwrap();
        let store = Store::load_from_dir(&dir);
        assert_eq!(store.recent().len(), MAX_RECENT);
        assert!(store.records().is_empty());
        assert!(
            !std::fs::read_to_string(dir.join("recent.tsv"))
                .unwrap()
                .contains(&key)
        );
        assert!(std::fs::read(dir.join("records.tsv")).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_only_links_never_become_records_or_history() {
        let dir = test_dir("privacy");
        let mut store = Store::load_from_dir(&dir);
        for raw in [
            "https://user:password@example.test/clip.mp4",
            "https://bearer@example.test/clip.mp4",
            "https://example.test/clip.mp4?provider-specific-field=private",
            "https://example.test/clip.mp4#access=private",
        ] {
            let uri = MediaUri::parse(raw);
            store.push_recent(&uri);
            assert!(!store.remember(&uri, 40., 120., false));
            assert!(!store.toggle_favorite(&uri));
        }
        assert!(store.recent().is_empty());
        assert!(store.records().is_empty());
        assert!(!store.is_dirty());
        assert!(!dir.join("records.tsv").exists());
        assert!(!dir.join("recent.tsv").exists());
        let public = MediaUri::parse("https://example.test/public/clip.mp4");
        store.push_recent(&public);
        assert!(store.remember(&public, 40., 120., false));
        assert!(store.toggle_favorite(&public));
        let reopened = Store::load_from_dir(&dir);
        assert_eq!(reopened.recent(), std::slice::from_ref(&public));
        assert_eq!(reopened.resume(&public), 40.);
        assert!(reopened.is_favorite(&public));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scrub_existing_files_and_retire_both_legacy_lists_before_clear() {
        let dir = test_dir("scrub");
        let unsafe_keys = [
            hex_str("https://a.test/1?ticket=private"),
            hex_str("https://secret@a.test/2"),
        ];
        let plugin = MediaUri::parse("iris-plugin://provider/source/item");
        let stable = hex_str(&plugin.to_uri_string());
        for (records, recent, key) in [
            ("records.tsv", "recent.tsv", &unsafe_keys[0]),
            ("uri_records.tsv", "recent_uris.tsv", &unsafe_keys[1]),
        ] {
            std::fs::write(
                dir.join(records),
                format!("{stable}\t1\t30\t90\n{key}\t1\t20\t100\n"),
            )
            .unwrap();
            std::fs::write(dir.join(recent), format!("{stable}\n{key}\n")).unwrap();
        }
        let mut store = Store::load_from_dir(&dir);
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.recent(), std::slice::from_ref(&plugin));
        assert!(store.is_favorite(&plugin));
        for filename in ["records.tsv", "recent.tsv"] {
            let contents = std::fs::read_to_string(dir.join(filename)).unwrap();
            assert!(contents.contains(&stable));
            assert!(
                unsafe_keys
                    .iter()
                    .all(|key| !contents.contains(key.as_str()))
            );
        }
        assert!(!dir.join("uri_records.tsv").exists());
        assert!(!dir.join("recent_uris.tsv").exists());
        store.clear_recent();
        assert!(store.remove_favorite(&plugin));
        assert!(store.reset_progress(&plugin));
        let reopened = Store::load_from_dir(&dir);
        assert!(reopened.records().is_empty());
        assert!(reopened.recent().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_flush_retains_edits_and_preserves_previous_data() {
        let dir = test_dir("retry");
        let first = MediaUri::parse("first.mkv");
        let second = MediaUri::parse("second.mkv");
        let mut store = Store::load_from_dir(&dir);
        assert!(store.toggle_favorite(&first));
        store.push_recent(&first);
        // Make the existing files unreadable as TSV without permission tricks or sleeps.
        for file in ["records", "recent"] {
            let path = dir.join(format!("{file}.tsv"));
            std::fs::rename(&path, dir.join(file)).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        assert!(store.remember(&second, 30., 90., false));
        store.push_recent(&second);
        assert!(store.is_dirty());
        assert!(store.remember(&second, 45., 90., false));
        for file in ["records", "recent"] {
            let path = dir.join(format!("{file}.tsv"));
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(dir.join(file), path).unwrap();
        }
        store.save();
        assert!(!store.is_dirty());
        let reopened = Store::load_from_dir(&dir);
        assert!(reopened.is_favorite(&first));
        assert_eq!(reopened.resume(&second), 45.);
        assert_eq!(reopened.recent(), &[second, first]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_instances_merge_fields_and_never_restore_unrelated_deletes() {
        let dir = test_dir("merge");
        let x = MediaUri::parse("x.mkv");
        let y = MediaUri::parse("y.mkv");
        let mut a = Store::load_from_dir(&dir);
        let mut b = Store::load_from_dir(&dir);
        assert!(a.remember(&x, 30., 100., false));
        assert!(b.toggle_favorite(&x));
        a.save();
        assert!(a.is_favorite(&x));
        assert!(b.toggle_favorite(&y));
        assert_eq!(b.resume(&x), 30.);
        a.push_recent(&x);
        b.push_recent(&y);
        assert_eq!(b.recent(), &[y.clone(), x.clone()]);
        b.clear_recent();
        a.push_recent(&y);
        assert!(b.reset_progress(&x));
        assert!(b.remove_favorite(&x));
        assert!(a.remember(&y, 50., 100., false));
        a.save();
        let reopened = Store::load_from_dir(&dir);
        assert!(!reopened.records().contains_key(&x.to_uri_string()));
        assert_eq!(reopened.recent(), std::slice::from_ref(&y));
        assert!(reopened.is_favorite(&y));
        assert_eq!(reopened.resume(&y), 50.);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn progress_pending_before_another_instances_reset_does_not_undo_it() {
        let dir = test_dir("pending-reset");
        let x = MediaUri::parse("episode.mkv");
        let mut first = Store::load_from_dir(&dir);
        assert!(first.remember(&x, 30., 100., false));
        first.save();
        let mut second = Store::load_from_dir(&dir);
        assert!(first.remember(&x, 35., 100., false));
        assert!(second.reset_progress(&x));
        first.save();
        assert_eq!(first.resume(&x), 0.);
        assert!(Store::load_from_dir(&dir).records().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
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

        // A synchronous flush acknowledges changes only after the write succeeds.
        let cloud_item = MediaUri::Plugin {
            plugin_id: "acme.cloud".into(),
            source_id: "drive".into(),
            entry_id: "ep2".into(),
        };
        assert!(store.remember(&cloud_item, 15.0, 60.0, false));
        assert!(store.is_dirty());
        store.save();
        assert!(!store.is_dirty());

        assert!(!dir.join("uri_records.tsv").exists());
        assert!(!dir.join("recent_uris.tsv").exists());
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
