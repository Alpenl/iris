//! stderr during development; bounded single-file diagnostics for GUI releases.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

struct LogOutput {
    file: File,
    remaining: u64,
}
impl Write for LogOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(bytes);
        // A long-running GUI must not grow logs without bound. Once the cap is
        // reached stderr still works; the next launch starts a fresh log.
        if self.remaining >= bytes.len() as u64 {
            self.file.write_all(bytes)?;
            self.remaining -= bytes.len() as u64;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub fn log_path() -> Option<PathBuf> {
    std::env::var_os("IRIS_LOG").map(PathBuf::from).or_else(|| {
        if let Some(dir) = std::env::var_os("IRIS_LOG_DIR").filter(|dir| !dir.is_empty()) {
            return Some(PathBuf::from(dir).join("iris.log"));
        }
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Iris"));
        #[cfg(target_os = "macos")]
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join("Library/Logs/Iris"));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let base: Option<PathBuf> = None;
        base.map(|p| p.join("iris.log"))
    })
}

pub fn init() {
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("iris=info,iris_core=info,iris_mpv=info"),
    );
    builder.format_timestamp_millis();
    if let Some(path) = log_path() {
        let opened = crate::fs_util::create_parent(&path).and_then(|_| {
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
        });
        match opened {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(LogOutput {
                    file,
                    remaining: 4 * 1024 * 1024,
                })));
            }
            Err(e) => {
                eprintln!("Iris: cannot open log {}: {e}", path.display());
            }
        }
    }
    builder.init();
}
