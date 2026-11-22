//! Path-keyed personal data, stored beside settings, never beside media.
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record {
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
}

const MAX_RECENT: usize = 20;

#[derive(Default)]
pub struct Store {
    pub records: BTreeMap<PathBuf, Record>,
    pub recent: Vec<PathBuf>,
    dirty: bool,
}

impl Store {
    pub fn load() -> Self {
        let Some(cfg) = crate::settings::path() else {
            return Self::default();
        };
        let mut this = Self::read(&cfg.with_file_name("records.tsv"));
        this.recent = Self::read_recent(&cfg.with_file_name("recent.tsv"));
        this
    }
    fn read_recent(path: &Path) -> Vec<PathBuf> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(p) = decode_path(line.trim())
                && !out.contains(&p)
            {
                out.push(p);
                if out.len() >= MAX_RECENT {
                    break;
                }
            }
        }
        out
    }
    fn save_recent(&self) {
        let Some(path) = crate::settings::path().map(|p| p.with_file_name("recent.tsv")) else {
            return;
        };
        if let Err(e) = self.write_recent(&path) {
            log::error!("saving recent list failed: {e}");
        }
    }
    fn write_recent(&self, path: &Path) -> std::io::Result<()> {
        let mut text = String::with_capacity(self.recent.len() * 64);
        for p in self.recent.iter().take(MAX_RECENT) {
            encode_path_into(p, &mut text);
            text.push('\n');
        }
        crate::settings::atomic_write(path, &text)
    }
    pub fn push_recent(&mut self, path: &Path) {
        let p = path.to_path_buf();
        self.recent.retain(|x| x != &p);
        self.recent.insert(0, p);
        self.recent.truncate(MAX_RECENT);
        self.save_recent();
    }
    pub fn clear_recent(&mut self) {
        if self.recent.is_empty() {
            return;
        }
        self.recent.clear();
        self.save_recent();
    }
    pub fn reset_progress(&mut self, path: &Path) -> bool {
        let Some(r) = self.records.get_mut(path) else {
            return false;
        };
        if r.position <= 0. && r.duration <= 0. {
            return false;
        }
        r.position = 0.;
        r.duration = 0.;
        if !r.favorite {
            self.records.remove(path);
        }
        self.save();
        true
    }
    fn read(path: &Path) -> Self {
        let mut this = Self::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let fields: Vec<_> = line.split('\t').collect();
                if fields.len() != 4 {
                    continue;
                }
                let (Some(path), Ok(position), Ok(duration)) = (
                    decode_path(fields[0]),
                    fields[2].parse::<f64>(),
                    fields[3].parse::<f64>(),
                ) else {
                    continue;
                };
                if !position.is_finite() || !duration.is_finite() || position < 0. || duration < 0.
                {
                    continue;
                }
                this.records.insert(
                    path,
                    Record {
                        favorite: fields[1] == "1",
                        position,
                        duration,
                    },
                );
            }
        }
        this
    }

    pub fn save(&mut self) {
        let Some(path) = crate::settings::path().map(|p| p.with_file_name("records.tsv")) else {
            return;
        };
        if let Err(e) = self.write(&path) {
            log::error!("saving records failed: {e}");
        } else {
            self.dirty = false;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Captures serialized records for off-UI-thread atomic saving, clearing the
    /// dirty flag. Returns `None` if nothing changed since the last save.
    pub fn take_dirty_snapshot(&mut self) -> Option<(PathBuf, String)> {
        if !self.dirty {
            return None;
        }
        let path = crate::settings::path().map(|p| p.with_file_name("records.tsv"))?;
        self.dirty = false;
        Some((path, self.format_records()))
    }

    /// Writes a previously captured snapshot to disk (safe to run on a worker thread).
    pub fn write_snapshot(path: &Path, text: &str) {
        if let Err(e) = crate::settings::atomic_write(path, text) {
            log::error!("saving records failed: {e}");
        }
    }

    fn format_records(&self) -> String {
        let mut text = String::with_capacity(self.records.len() * 96);
        for (p, r) in &self.records {
            encode_path_into(p, &mut text);
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

    fn write(&self, path: &Path) -> std::io::Result<()> {
        let text = self.format_records();
        crate::settings::atomic_write(path, &text)
    }

    pub fn favorite(&self, path: &Path) -> bool {
        self.records.get(path).is_some_and(|r| r.favorite)
    }
    pub fn toggle_favorite(&mut self, path: &Path) -> bool {
        let r = self.records.entry(path.to_path_buf()).or_default();
        r.favorite = !r.favorite;
        let favorite = r.favorite;
        if !favorite && r.position <= 0. && r.duration <= 0. {
            self.records.remove(path);
        }
        self.save();
        favorite
    }
    pub fn remove_favorite(&mut self, path: &Path) -> bool {
        if let Some(r) = self.records.get_mut(path)
            && r.favorite
        {
            r.favorite = false;
            if r.position <= 0. && r.duration <= 0. {
                self.records.remove(path);
            }
            self.save();
            return true;
        }
        false
    }
    pub fn progress_ratio(&self, path: &Path) -> Option<f32> {
        let r = self.records.get(path)?;
        if r.duration > 0. && r.position >= 5. && r.position < r.duration - 5. {
            Some((r.position / r.duration).clamp(0., 1.) as f32)
        } else {
            None
        }
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
    pub fn resume(&self, path: &Path) -> f64 {
        self.records
            .get(path)
            .filter(|r| r.position >= 5. && r.position < r.duration - 5.)
            .map_or(0., |r| r.position)
    }
    pub fn remember(&mut self, path: &Path, position: f64, duration: f64, eof: bool) -> bool {
        if !position.is_finite() || !duration.is_finite() || duration <= 0. {
            return false;
        }
        if eof {
            let changed = match self.records.get_mut(path) {
                Some(r) if r.favorite => {
                    let diff = r.position > 0. || (r.duration - duration).abs() > 0.01;
                    r.position = 0.;
                    r.duration = duration;
                    diff
                }
                Some(_) => {
                    self.records.remove(path);
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
        if let Some(r) = self.records.get_mut(path) {
            if (r.position - next_pos).abs() < 0.25 && (r.duration - duration).abs() < 0.01 {
                return false;
            }
            r.position = next_pos;
            r.duration = duration;
            self.dirty = true;
            return true;
        }
        // Avoid storing unresumable non-favorite positions near start/end.
        if next_pos < 5. || next_pos >= duration - 5. {
            return false;
        }
        self.records.insert(
            path.to_path_buf(),
            Record {
                favorite: false,
                position: next_pos,
                duration,
            },
        );
        self.dirty = true;
        true
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn push_hex(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
}

fn encode_path_into(path: &Path, out: &mut String) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        push_hex(path.as_os_str().as_bytes(), out);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            push_hex(&unit.to_le_bytes(), out);
        }
    }
}

#[cfg(test)]
fn encode_path(path: &Path) -> String {
    let mut out = String::new();
    encode_path_into(path, &mut out);
    out
}

fn decode_path(text: &str) -> Option<PathBuf> {
    if !text.len().is_multiple_of(2) || text.len() > 262144 || !text.is_ascii() {
        return None;
    }
    let bytes: Vec<u8> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(std::ffi::OsString::from_vec(bytes).into())
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
        Some(std::ffi::OsString::from_wide(&units).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_with_delimiters_roundtrip() {
        let p = Path::new("/素材/hello\tworld\n100%.mp4");
        assert_eq!(decode_path(&encode_path(p)).as_deref(), Some(p));
        assert_eq!(decode_path("💡"), None);
        assert_eq!(decode_path("zz"), None);
    }
    #[test]
    fn records_survive_replacement_and_ignore_corrupt_rows() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("iris-records-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("records.tsv");
        let media = Path::new("/素材/a\tb.mp4");
        let mut store = Store::default();
        store.records.insert(
            media.into(),
            Record {
                favorite: true,
                position: 10.,
                duration: 60.,
            },
        );
        store.write(&file).unwrap();
        store.remember(media, 25., 60., false);
        store.write(&file).unwrap();
        let mut damaged = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(damaged, "invalid\t1\tNaN\t60").unwrap();
        drop(damaged);
        let restored = Store::read(&file);
        assert_eq!(restored.records.len(), 1);
        assert_eq!(restored.resume(media), 25.);
        assert!(restored.favorite(media));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn resume_skips_completed_files_without_losing_favorites() {
        let p = Path::new("test.mp4");
        let mut s = Store::default();
        s.records.insert(
            p.into(),
            Record {
                favorite: true,
                position: 30.,
                duration: 60.,
            },
        );
        assert_eq!(s.resume(p), 30.);
        s.remember(p, 60., 60., true);
        assert_eq!(s.resume(p), 0.);
        assert!(s.favorite(p));
    }
    #[test]
    fn progress_ratio_and_clear_progress_preserve_favorites() {
        let fav = Path::new("fav.mp4");
        let plain = Path::new("plain.mp4");
        let mut s = Store::default();
        s.records.insert(
            fav.into(),
            Record {
                favorite: true,
                position: 30.,
                duration: 60.,
            },
        );
        s.records.insert(
            plain.into(),
            Record {
                favorite: false,
                position: 15.,
                duration: 60.,
            },
        );
        assert_eq!(s.progress_ratio(fav), Some(0.5));
        assert_eq!(s.progress_ratio(plain), Some(0.25));
        // Clear progress keeps fav's star and removes non-favorite progress-only row.
        assert_eq!(s.clear_progress(), 2);
        assert!(s.favorite(fav));
        assert_eq!(s.progress_ratio(fav), None);
        assert!(!s.records.contains_key(plain));
    }

    #[test]
    fn remember_prunes_completed_non_favorites_and_tracks_dirty() {
        let p = Path::new("clip.mp4");
        let mut s = Store::default();
        assert!(!s.is_dirty());
        // Within first 5s: not resumable, should not create a dead entry.
        assert!(!s.remember(p, 2.0, 60.0, false));
        assert!(!s.records.contains_key(p));
        assert!(!s.is_dirty());
        // Resumable position creates entry and marks dirty.
        assert!(s.remember(p, 15.0, 60.0, false));
        assert!(s.is_dirty());
        assert_eq!(s.resume(p), 15.0);
        // Tiny sub-0.25s drift does not trigger another write.
        assert!(!s.remember(p, 15.1, 60.0, false));
        // Reaching EOF removes non-favorite entry cleanly.
        assert!(s.remember(p, 60.0, 60.0, true));
        assert!(!s.records.contains_key(p));
    }
}
