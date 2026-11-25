//! Size-bounded rotating file writer that preserves prior sessions and degrades gracefully on I/O errors.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(super) const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const DEFAULT_MAX_BACKUPS: usize = 5;

struct RotatingState {
    file: Option<File>,
    written: u64,
}

/// Thread-safe size-bounded rotating file sink (`name.ext` -> `name.1.ext` .. `name.N.ext`).
pub(super) struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    max_backups: usize,
    state: Mutex<RotatingState>,
}

impl RotatingFile {
    pub(super) fn open(
        path: PathBuf,
        max_bytes: u64,
        max_backups: usize,
        rotate_existing_on_open: bool,
    ) -> io::Result<Self> {
        crate::fs_util::create_parent(&path)?;
        if rotate_existing_on_open && fs::metadata(&path).is_ok_and(|meta| meta.len() > 0) {
            rotate_backups(&path, max_backups);
        }
        let existing_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if existing_len >= max_bytes {
            rotate_backups(&path, max_backups);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            max_bytes: max_bytes.max(1024),
            max_backups,
            state: Mutex::new(RotatingState {
                file: Some(file),
                written,
            }),
        })
    }

    pub(super) fn write_bytes(&self, bytes: &[u8], flush: bool) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.written > 0 && guard.written.saturating_add(bytes.len() as u64) > self.max_bytes {
            if let Some(mut f) = guard.file.take() {
                let _ = f.flush();
            }
            rotate_backups(&self.path, self.max_backups);
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&self.path)?;
            guard.file = Some(f);
            guard.written = 0;
        }
        if guard.file.is_none() {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            guard.written = f.metadata().map(|m| m.len()).unwrap_or(0);
            guard.file = Some(f);
        }
        if let Some(file) = guard.file.as_mut() {
            file.write_all(bytes)?;
            if flush {
                file.flush()?;
            }
        }
        guard.written = guard.written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    pub(super) fn flush(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = guard.file.as_mut() {
            let _ = file.flush();
        }
    }
}

pub(super) fn backup_path(base: &Path, index: usize) -> PathBuf {
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("iris");
    let ext = base.extension().and_then(|s| s.to_str());
    let file_name = match ext {
        Some(ext) if !ext.is_empty() => format!("{stem}.{index}.{ext}"),
        _ => format!("{stem}.{index}"),
    };
    base.with_file_name(file_name)
}

fn rotate_backups(base: &Path, max_backups: usize) {
    if max_backups == 0 {
        let _ = fs::remove_file(base);
        return;
    }
    let oldest = backup_path(base, max_backups);
    let _ = fs::remove_file(&oldest);
    for idx in (1..max_backups).rev() {
        let from = backup_path(base, idx);
        let to = backup_path(base, idx + 1);
        if from.exists() {
            let _ = fs::remove_file(&to);
            let _ = fs::rename(&from, &to);
        }
    }
    if base.exists() {
        let first = backup_path(base, 1);
        let _ = fs::remove_file(&first);
        let _ = fs::rename(base, &first);
    }
}
