//! Shared filesystem helpers: application data directory lookup and atomic file writes.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns the application data directory:
/// - Parent directory of `IRIS_SETTINGS` when explicitly set;
/// - macOS: `~/Library/Application Support/Iris`;
/// - Windows: `%APPDATA%\Iris`;
/// - Other Unix: `$XDG_CONFIG_HOME/iris` or `~/.config/iris`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_SETTINGS") {
        let dir = Path::new(&p).parent()?;
        return if dir.as_os_str().is_empty() {
            std::env::current_dir().ok()
        } else if dir.is_relative() {
            std::env::current_dir().ok().map(|cwd| cwd.join(dir))
        } else {
            Some(dir.to_path_buf())
        };
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Iris"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Iris"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("iris"))
    }
}

/// Keep the lock on a stable sibling: the data files themselves are replaced by rename.
/// Closing the returned file releases the OS lock, including on process exit.
pub(crate) fn lock_file(path: &Path) -> io::Result<std::fs::File> {
    lock_file_with_timeout(path, Duration::from_millis(20))
}

pub(crate) fn lock_file_with_timeout(path: &Path, timeout: Duration) -> io::Result<std::fs::File> {
    create_parent(path)?;
    let lock = retry_sharing_violation(|| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    })?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                std::thread::sleep(Duration::from_millis(5).min(deadline - now));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

pub(crate) fn create_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub(crate) fn retry_sharing_violation<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for attempt in 1.. {
        match op() {
            Err(e) if cfg!(windows) && attempt < 5 && windows_sharing_violation(&e) => {
                std::thread::sleep(Duration::from_millis(10 * attempt));
            }
            result => return result,
        }
    }
    unreachable!()
}

fn windows_sharing_violation(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33 | 1224))
}

/// Atomically writes `bytes` to `path` via a unique sibling `.tmp` file,
/// cleaning up the temp file on failure and retrying briefly on Windows
/// transient sharing locks.
pub(crate) fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_impl(path, bytes.as_ref(), false)
}

/// Atomically writes sensitive `bytes` to `path` with owner-only `0600` permissions
/// on Unix, with data synchronized before rename and sharing-lock retries on Windows.
pub(crate) fn atomic_write_private(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_impl(path, bytes.as_ref(), true)
}

fn write_impl(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let Some(base_name) = path.file_name().filter(|n| !n.is_empty()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write target has no file name",
        ));
    };
    create_parent(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut opened = None;
    for _ in 0..64 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = base_name.to_os_string();
        tmp_name.push(format!(".{}.{}.tmp", std::process::id(), seq));
        let tmp = path.with_file_name(tmp_name);
        match retry_sharing_violation(|| opts.open(&tmp)) {
            Ok(file) => {
                opened = Some((tmp, file));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((tmp, mut file)) = opened else {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "exhausted atomic temp file retries",
        ));
    };
    let write = file.write_all(bytes).and_then(|()| {
        if private {
            file.sync_all()
        } else {
            file.sync_data()
        }
    });
    drop(file);
    let result = write.and_then(|()| retry_sharing_violation(|| std::fs::rename(&tmp, path)));
    if result.is_err() {
        let _ = retry_sharing_violation(|| std::fs::remove_file(&tmp));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn retries_windows_sharing_errors() {
        for code in [32, 33, 1224] {
            let mut calls = 0;
            retry_sharing_violation(|| {
                calls += 1;
                if calls == 1 {
                    Err(io::Error::from_raw_os_error(code))
                } else {
                    Ok(())
                }
            })
            .unwrap();
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn atomic_write_and_private_mode_roundtrip() {
        let dir = std::env::temp_dir().join(format!("iris-fs-util-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let pub_file = dir.join("nested/public.txt");
        atomic_write(&pub_file, "hello").unwrap();
        atomic_write(&pub_file, "world").unwrap();
        assert_eq!(std::fs::read_to_string(&pub_file).unwrap(), "world");

        let priv_file = dir.join("nested/secret.json");
        atomic_write_private(&priv_file, b"{\"ok\":true}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&priv_file).unwrap(),
            "{\"ok\":true}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&priv_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_rejects_nameless_paths_immediately() {
        for bad in ["", ".", "..", "/"] {
            let err = atomic_write(Path::new(bad), b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }
}
