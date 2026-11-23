//! Shared filesystem helpers: application data directory lookup and atomic file writes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns the application data directory:
/// - Parent directory of `IRIS_SETTINGS` when explicitly set;
/// - macOS: `~/Library/Application Support/Iris`;
/// - Windows: `%APPDATA%\Iris`;
/// - Other Unix: `$XDG_CONFIG_HOME/iris` or `~/.config/iris`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_SETTINGS") {
        let pb = PathBuf::from(p);
        return pb.parent().map(Path::to_path_buf);
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
pub(crate) fn lock_store(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("records.lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// Atomically writes `bytes` to `path` via a unique sibling `.tmp` file,
/// cleaning up the temp file on failure and retrying briefly on Windows
/// transient sharing locks.
pub(crate) fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_impl(path, bytes.as_ref(), false)
}

/// Atomically writes sensitive `bytes` to `path` with owner-only `0600` permissions
/// on Unix (`fsync` before rename) and transient sharing-lock retries on Windows.
pub(crate) fn atomic_write_private(path: &Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_impl(path, bytes.as_ref(), true)
}

fn write_impl(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), seq));

    let write_res = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        if private {
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let attempts = if cfg!(windows) { 5 } else { 1 };
    let mut last_err = None;
    for attempt in 0..attempts {
        match std::fs::rename(&tmp, path) {
            Ok(()) => {
                #[cfg(unix)]
                if private {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                return Ok(());
            }
            Err(e)
                if cfg!(windows)
                    && attempt + 1 < attempts
                    && e.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1) as u64));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last_err.unwrap_or_else(|| std::io::Error::other("atomic rename failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
