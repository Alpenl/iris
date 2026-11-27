//! Unified URI-keyed personal records and recent media list (`records.tsv`, `recent.tsv`).
//! Stored in the application data directory, never beside media files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use crate::concurrency::CancelToken;
use crate::fs_util;
use crate::media_uri::MediaUri;
mod annotations;
mod recent;
mod tsv;
pub use annotations::{
    AnnotationEdit, AnnotationError, AnnotationPage, AnnotationSnapshot, AnnotationTarget,
    Annotations, ColorLabel, TimeMarker,
};
use recent::read_recent;
use tsv::{canonical_hex_matches, decode_any_media_uri, push_hex, read_tsv};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record {
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
}

const MAX_RECORDS: usize = 4096;
const MAX_RECORD_KEY: usize = 128 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
// Two fixed-point f64 fields (up to 313 bytes each), flag and separators.
const MAX_RECORD_FIELDS: usize = 640;

type Records = BTreeMap<String, Record>;

#[derive(Clone, Debug, Default)]
struct RecordEdit {
    favorite: Option<bool>,
    progress_before: Option<Option<(f64, f64)>>,
    progress: Option<(f64, f64)>,
}

pub struct ProgressFlush {
    dir: PathBuf,
    edits: BTreeMap<String, RecordEdit>,
    cancel: CancelToken,
}

impl ProgressFlush {
    pub fn run(self) -> Option<BTreeMap<String, (f64, f64)>> {
        if self.cancel.is_cancelled() {
            return None;
        }
        let _lock = fs_util::lock_file(&self.dir.join("records.lock")).ok()?;
        if self.cancel.is_cancelled() {
            return None;
        }
        let path = self.dir.join("records.tsv");
        let mut records = Records::new();
        let (_, scrub) = Store::read_records(&path, &mut records).ok()?;
        let mut changed = scrub;
        let mut applied = BTreeMap::new();
        for (key, edit) in &self.edits {
            let Some((position, duration)) = edit.progress else {
                continue;
            };
            let old = records.get(key).copied();
            let mut record = old.unwrap_or_default();
            if edit.matches_progress(old) {
                record.position = position;
                record.duration = duration;
                applied.insert(key.clone(), (position, duration));
            }
            if record == Record::default() {
                changed |= records.remove(key).is_some();
            } else if old != Some(record) {
                records.insert(key.clone(), record);
                changed = true;
            }
        }
        changed |= prune_records(&mut records, &self.edits);
        if changed {
            Store::write_records(&path, &records).ok()?;
        }
        Some(applied)
    }
}

impl RecordEdit {
    fn remember_progress(&mut self, before: Option<(f64, f64)>, after: (f64, f64)) {
        if self.progress.is_none() {
            self.progress_before = Some(before);
        }
        self.progress = Some(after);
    }

    fn matches_progress(&self, old: Option<Record>) -> bool {
        // TSV rounds to milliseconds. A conflicting on-disk edit, including a
        // reset by another window while this save was pending, takes precedence.
        match self.progress_before {
            None => true,
            Some(None) => old.is_none_or(|record| {
                record.position.abs() <= 0.000_501 && record.duration.abs() <= 0.000_501
            }),
            Some(Some((pos, dur))) => old.is_some_and(|record| {
                (record.position - pos).abs() <= 0.000_501
                    && (record.duration - dur).abs() <= 0.000_501
            }),
        }
    }
}

#[derive(Default, Debug)]
pub struct Store {
    annotations: annotations::AnnotationStore,
    records: BTreeMap<String, Record>,
    recent: Vec<MediaUri>,
    recent_titles: BTreeMap<String, String>,
    edits: BTreeMap<String, RecordEdit>,
    in_flight: BTreeMap<String, (f64, f64)>,
    in_flight_abort: Option<CancelToken>,
    recent_pushes: Vec<(MediaUri, Option<String>)>,
    recent_title_updates: BTreeMap<String, (String, bool)>,
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
            annotations: annotations::AnnotationStore::load(dir),
            backing_dir: Some(dir.to_path_buf()),
            refresh_records: true,
            refresh_recent: true,
            ..Self::default()
        };
        match fs_util::lock_file(&dir.join("records.lock")) {
            Ok(_lock) => store.sync(dir),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Each canonical file is atomically replaced. Read the last complete
                // snapshots now, and defer any migration or writes until we own the lock.
                if let Err(e) = Self::read_records(&dir.join("records.tsv"), &mut store.records) {
                    log::error!("reading records during lock contention: {e}");
                }
                if let Err(e) = read_recent(
                    &dir.join("recent.tsv"),
                    &mut store.recent,
                    &mut store.recent_titles,
                ) {
                    log::error!("reading recent list during lock contention: {e}");
                }
            }
            Err(e) => log::error!("locking media store failed: {e}"),
        }
        store
    }

    fn target_dir(&self) -> Option<PathBuf> {
        self.backing_dir.clone().or_else(fs_util::data_dir)
    }

    pub fn records(&self) -> &BTreeMap<String, Record> {
        &self.records
    }
    pub fn annotations(&self, uri: &MediaUri) -> Option<&Annotations> {
        self.annotations.get(uri)
    }
    pub fn annotation_target(&self) -> Result<AnnotationTarget, AnnotationError> {
        Ok(AnnotationTarget::new(
            self.target_dir().ok_or(AnnotationError::Io)?,
        ))
    }
    pub fn apply_annotation_snapshot(
        &mut self,
        snapshot: AnnotationSnapshot,
    ) -> Result<(), AnnotationError> {
        self.annotations.accept(snapshot);
        Ok(())
    }

    pub fn favorites(&self) -> Vec<MediaUri> {
        self.records
            .iter()
            .filter(|(_, r)| r.favorite)
            .map(|(k, _)| MediaUri::parse(k))
            .collect()
    }

    fn read_records(path: &Path, out: &mut Records) -> io::Result<(bool, bool)> {
        let mut bytes = record_bytes(out);
        read_tsv(path, |line| {
            let mut fields = line.split('\t');
            let (Some(raw), Some(favorite @ ("0" | "1")), Some(position), Some(duration), None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return true;
            };
            let (Some(uri), Ok(position), Ok(duration)) = (
                decode_any_media_uri(raw),
                position.parse::<f64>(),
                duration.parse::<f64>(),
            ) else {
                return true;
            };
            if !uri.may_share_identity()
                || !position.is_finite()
                || !duration.is_finite()
                || position < 0.
                || duration < 0.
            {
                return true;
            }
            let key = uri.to_uri_string();
            if key.is_empty() || key.len() > MAX_RECORD_KEY {
                return true;
            }
            let mut scrub = !canonical_hex_matches(raw, &key);
            let cost = key.len() * 2 + MAX_RECORD_FIELDS;
            if out
                .insert(
                    key,
                    Record {
                        favorite: favorite == "1",
                        position,
                        duration,
                    },
                )
                .is_none()
            {
                bytes += cost;
            }
            if bytes > MAX_RECORD_BYTES * 2 || out.len() > MAX_RECORDS * 2 {
                scrub |= prune_records(out, &BTreeMap::new());
                bytes = record_bytes(out);
            }
            scrub
        })
    }

    fn abort_in_flight_flush(&mut self) {
        if let Some(abort) = self.in_flight_abort.take() {
            abort.cancel();
        }
    }

    pub fn save(&mut self) {
        if !self.is_dirty() {
            return;
        }
        self.abort_in_flight_flush();
        let Some(dir) = self.target_dir() else { return };
        let _lock = match fs_util::lock_file(&dir.join("records.lock")) {
            Ok(lock) => lock,
            Err(e) => {
                log::error!("locking media store failed: {e}");
                return;
            }
        };
        self.sync(&dir);
    }

    pub fn prepare_progress_flush(&mut self) -> Option<ProgressFlush> {
        if self.refresh_records
            || self.refresh_recent
            || self.clear_all_progress
            || self.clear_all_recent
            || !self.recent_pushes.is_empty()
            || !self.recent_title_updates.is_empty()
            || self.edits.is_empty()
            || self.edits.values().any(|e| e.favorite.is_some())
            || self.dir_has_legacy()?
        {
            return None;
        }
        let dir = self.target_dir()?;
        self.abort_in_flight_flush();
        for (k, e) in &self.edits {
            if let Some(prog) = e.progress {
                self.in_flight.insert(k.clone(), prog);
            }
        }
        let cancel = CancelToken::new();
        self.in_flight_abort = Some(cancel.clone());
        Some(ProgressFlush {
            dir,
            edits: self.edits.clone(),
            cancel,
        })
    }

    fn dir_has_legacy(&self) -> Option<bool> {
        let dir = self.target_dir()?;
        Some(dir.join("uri_records.tsv").exists())
    }

    pub fn finish_progress_flush(&mut self, applied: Option<BTreeMap<String, (f64, f64)>>) {
        self.in_flight_abort = None;
        let Some(applied) = applied else {
            self.in_flight.clear();
            return;
        };
        for (key, done) in applied {
            self.in_flight.remove(&key);
            let remove = if let Some(edit) = self.edits.get_mut(&key) {
                if edit.favorite.is_none() && edit.progress == Some(done) {
                    true
                } else {
                    edit.progress_before = Some(Some(done));
                    false
                }
            } else {
                false
            };
            if remove {
                self.edits.remove(&key);
            }
        }
    }

    fn sync(&mut self, dir: &Path) {
        if (self.refresh_records || self.clear_all_progress || !self.edits.is_empty())
            && let Err(e) = self.sync_records(dir)
        {
            log::error!("synchronizing records failed: {e}");
        }
        if (self.refresh_recent
            || self.clear_all_recent
            || !self.recent_pushes.is_empty()
            || !self.recent_title_updates.is_empty())
            && let Err(e) = self.sync_recent(dir)
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
            || !self.recent_title_updates.is_empty()
    }

    fn sync_records(&mut self, dir: &Path) -> io::Result<()> {
        let path = dir.join("records.tsv");
        let legacy_path = dir.join("uri_records.tsv");
        let mut records = Records::new();
        let (legacy, _) = Self::read_records(&legacy_path, &mut records)?;
        let (_, scrub) = Self::read_records(&path, &mut records)?;
        if legacy {
            // Commit the merged base and retire the old file before accepting deletes.
            // Repeating this pair after interruption is idempotent.
            prune_records(&mut records, &self.edits);
            Self::write_records(&path, &records)?;
            fs_util::retry_sharing_violation(|| std::fs::remove_file(&legacy_path))?;
        }
        let mut changed = scrub && !legacy;
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
                && (edit.matches_progress(old)
                    || self.in_flight.get(key).is_some_and(|&(pos, dur)| {
                        old.is_some_and(|r| {
                            (r.position - pos).abs() <= 0.000_501
                                && (r.duration - dur).abs() <= 0.000_501
                        })
                    }))
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
        changed |= prune_records(&mut records, &self.edits);
        if changed {
            Self::write_records(&path, &records)?;
        }
        self.records = records;
        self.edits.clear();
        self.in_flight.clear();
        self.clear_all_progress = false;
        self.refresh_records = false;
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

    pub fn is_favorite(&self, uri: &MediaUri) -> bool {
        self.records
            .get(&uri.to_uri_string())
            .is_some_and(|r| r.favorite)
    }

    pub fn toggle_favorite(&mut self, uri: &MediaUri) -> bool {
        if !uri.may_share_identity() {
            return false;
        }
        let key = uri.to_uri_string();
        if key.is_empty() || key.len() > MAX_RECORD_KEY {
            return false;
        }
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

    pub fn progress_ratio(&self, uri: &MediaUri) -> Option<f32> {
        let r = self.records.get(&uri.to_uri_string())?;
        if r.duration > 0. && r.position >= 5. && r.position < r.duration - 5. {
            Some((r.position / r.duration).clamp(0., 1.) as f32)
        } else {
            None
        }
    }

    pub fn resume(&self, uri: &MediaUri) -> f64 {
        self.records
            .get(&uri.to_uri_string())
            .filter(|r| r.position >= 5. && r.position < r.duration - 5.)
            .map_or(0., |r| r.position)
    }

    pub fn resume_path(&self, path: &Path) -> f64 {
        self.resume(&MediaUri::from(path))
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
        if key.is_empty() || key.len() > MAX_RECORD_KEY {
            return false;
        }
        let previous = self.records.get(&key).copied();
        let before = previous.map(|r| (r.position, r.duration));
        if eof {
            let Some(r) = previous else { return false };
            if r.favorite && r.position == 0. && r.duration == 0. {
                return false;
            }
            self.abort_in_flight_flush();
            self.edits
                .entry(key.clone())
                .or_default()
                .remember_progress(before, (0., 0.));
            if r.favorite {
                self.records.insert(
                    key,
                    Record {
                        position: 0.,
                        duration: 0.,
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
        self.remember(&MediaUri::from(path), position, duration, eof)
    }

    pub fn reset_progress(&mut self, uri: &MediaUri) -> bool {
        let key = uri.to_uri_string();
        let Some(r) = self.records.get(&key).copied() else {
            return false;
        };
        if r.position <= 0. && r.duration <= 0. {
            return false;
        }
        self.abort_in_flight_flush();
        if r.favorite {
            self.records.insert(
                key.clone(),
                Record {
                    position: 0.,
                    duration: 0.,
                    ..r
                },
            );
        } else {
            self.records.remove(&key);
        }
        let edit = self.edits.entry(key).or_default();
        edit.progress = Some((0., 0.));
        edit.progress_before = None;
        self.save();
        true
    }

    pub fn clear_progress(&mut self) -> usize {
        self.abort_in_flight_flush();
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

fn record_bytes(records: &Records) -> usize {
    records
        .keys()
        .map(|key| key.len() * 2 + MAX_RECORD_FIELDS)
        .sum()
}

fn prune_records(records: &mut Records, edits: &BTreeMap<String, RecordEdit>) -> bool {
    let mut bytes = record_bytes(records);
    let mut count = records.len();
    if count <= MAX_RECORDS && bytes <= MAX_RECORD_BYTES {
        return false;
    }
    // Drop unedited progress before favorites or records changed in this transaction.
    // Existing TSV rows have no timestamps, so use canonical key order as the tie breaker.
    for (favorite, edited) in [(false, false), (false, true), (true, false), (true, true)] {
        records.retain(|key, record| {
            if (count > MAX_RECORDS || bytes > MAX_RECORD_BYTES)
                && record.favorite == favorite
                && edits.contains_key(key) == edited
            {
                count -= 1;
                bytes -= key.len() * 2 + MAX_RECORD_FIELDS;
                false
            } else {
                true
            }
        });
        if count <= MAX_RECORDS && bytes <= MAX_RECORD_BYTES {
            break;
        }
    }
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

#[cfg(test)]
mod tests;
