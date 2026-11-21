//! Path-keyed personal data, stored beside settings, never beside media.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record {
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
}

#[derive(Default)]
pub struct Store {
    pub records: BTreeMap<PathBuf, Record>,
}

impl Store {
    pub fn load() -> Self {
        crate::settings::path()
            .map(|p| Self::read(&p.with_file_name("records.tsv")))
            .unwrap_or_default()
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

    pub fn save(&self) {
        let Some(path) = crate::settings::path().map(|p| p.with_file_name("records.tsv")) else {
            return;
        };
        if let Err(e) = self.write(&path) {
            log::error!("saving records failed: {e}");
        }
    }
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let text: String = self
            .records
            .iter()
            .map(|(p, r)| {
                format!(
                    "{}\t{}\t{:.3}\t{:.3}\n",
                    encode_path(p),
                    u8::from(r.favorite),
                    r.position,
                    r.duration
                )
            })
            .collect();
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&tmp, text)?;
            std::fs::rename(tmp, path)
        };
        write()
    }

    pub fn favorite(&self, path: &Path) -> bool {
        self.records.get(path).is_some_and(|r| r.favorite)
    }
    pub fn toggle_favorite(&mut self, path: &Path) -> bool {
        let r = self.records.entry(path.to_path_buf()).or_default();
        r.favorite = !r.favorite;
        let favorite = r.favorite;
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
    pub fn remember(&mut self, path: &Path, position: f64, duration: f64, eof: bool) {
        if !position.is_finite() || !duration.is_finite() || duration <= 0. {
            return;
        }
        let r = self.records.entry(path.to_path_buf()).or_default();
        r.position = if eof { 0. } else { position.max(0.) };
        r.duration = duration;
    }
}

fn encode_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes: Vec<u8> = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
}
