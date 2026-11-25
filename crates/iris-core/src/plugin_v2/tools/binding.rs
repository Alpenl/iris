use crate::{plugin_v2::files::identity::PinnedFile, process_tree, vfs::HttpCancel};
use iris_api::v2::ErrorCode;
use std::{path::Path, process::Command, sync::Arc, time::Duration};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Ffmpeg,
    Ffprobe,
    WhisperCpp,
    YtDlp,
}
impl ToolKind {
    pub fn supported_version(self) -> &'static str {
        match self {
            Self::Ffmpeg | Self::Ffprobe => "8.0.1",
            Self::WhisperCpp => "1.8.2",
            Self::YtDlp => "2025.11.12",
        }
    }
}
/// Obtained from a host-reviewed release receipt, not guest configuration. For
/// whisper.cpp the release SHA256 is the version evidence: its CLI has no version
/// query. The receipt must include approved non-system libraries/helpers too.
pub struct ToolApproval {
    pub kind: ToolKind,
    pub version: String,
    pub executable_sha256: [u8; 32],
    pub components: Vec<(std::path::PathBuf, [u8; 32])>,
}
pub struct TrustedTool {
    pub kind: ToolKind,
    pub version: String,
    executable: PinnedFile,
    digest: [u8; 32],
    components: Vec<(PinnedFile, [u8; 32])>,
}
impl TrustedTool {
    /// Blocking background validation after host confirmation. No program is
    /// executed before checking the approved digest and supported version.
    pub fn approve(
        path: &Path,
        approval: ToolApproval,
        cwd: &Path,
        cancel: &HttpCancel,
    ) -> Result<Arc<Self>, ErrorCode> {
        if approval.version != approval.kind.supported_version() || approval.components.len() > 256
        {
            return Err(ErrorCode::Unsupported);
        }
        let executable = PinnedFile::open(path)?;
        if executable.initial.size > 512 << 20
            || executable.sha256(cancel)? != approval.executable_sha256
        {
            return Err(ErrorCode::Denied);
        }
        let mut components = Vec::new();
        for (path, digest) in approval.components {
            let file = PinnedFile::open(&path)?;
            if file.initial.size > 2 << 30 || file.sha256(cancel)? != digest {
                return Err(ErrorCode::Denied);
            }
            components.push((file, digest));
        }
        let this = Arc::new(Self {
            kind: approval.kind,
            version: approval.version,
            executable,
            digest: approval.executable_sha256,
            components,
        });
        this.validate_layout()?;
        if this.kind != ToolKind::WhisperCpp {
            let probe_dir = cwd.join(format!(
                ".probe-{}",
                crate::plugin_v2::files::random_name()?
            ));
            std::fs::create_dir(&probe_dir).map_err(|_| ErrorCode::IoError)?;
            struct ProbeGuard(std::path::PathBuf);
            impl Drop for ProbeGuard {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let _guard = ProbeGuard(probe_dir.clone());
            let mut cmd = this.command(&probe_dir);
            cmd.arg(if this.kind == ToolKind::YtDlp {
                "--version"
            } else {
                "-version"
            });
            let output = process_tree::capture(&mut cmd, cancel, Duration::from_secs(10), 65536)?;
            let line = std::str::from_utf8(&output)
                .map_err(|_| ErrorCode::Unsupported)?
                .lines()
                .next()
                .unwrap_or("");
            let version = match this.kind {
                ToolKind::YtDlp => line.trim(),
                ToolKind::Ffmpeg => line
                    .strip_prefix("ffmpeg version ")
                    .and_then(|s| s.split_whitespace().next())
                    .ok_or(ErrorCode::Unsupported)?,
                ToolKind::Ffprobe => line
                    .strip_prefix("ffprobe version ")
                    .and_then(|s| s.split_whitespace().next())
                    .ok_or(ErrorCode::Unsupported)?,
                ToolKind::WhisperCpp => unreachable!(),
            };
            // Build suffixes are covered by the exact approved file digest.
            if version.split('-').next() != Some(this.version.as_str()) {
                return Err(ErrorCode::Unsupported);
            }
        }
        Ok(this)
    }
    pub fn revalidate(&self, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        self.validate_layout()?;
        if self.executable.sha256(cancel)? != self.digest {
            return Err(ErrorCode::StaleHandle);
        }
        // Check the path still names the held file as well as checking content.
        let current = PinnedFile::open(&self.executable.path)?;
        if current.initial != self.executable.initial {
            return Err(ErrorCode::StaleHandle);
        }
        for (file, digest) in &self.components {
            if file.sha256(cancel)? != *digest
                || PinnedFile::open(&file.path)?.initial != file.initial
            {
                return Err(ErrorCode::StaleHandle);
            }
        }
        Ok(())
    }
    fn validate_layout(&self) -> Result<(), ErrorCode> {
        let parent = self.executable.path.parent().ok_or(ErrorCode::Denied)?;
        let mut directories = vec![(parent.to_owned(), 0usize)];
        let approved: std::collections::BTreeSet<_> = self
            .components
            .iter()
            .map(|(file, _)| file.path.clone())
            .collect();
        #[cfg(windows)]
        if self.kind == ToolKind::YtDlp && !parent.join("_internal").is_dir() {
            // One-file PyInstaller extracts an unbounded private runtime before
            // normal option parsing. Only the reviewed onedir release is used.
            return Err(ErrorCode::Unsupported);
        }
        let mut count = 0;
        while let Some((directory, depth)) = directories.pop() {
            for entry in std::fs::read_dir(&directory).map_err(|_| ErrorCode::IoError)? {
                let entry = entry.map_err(|_| ErrorCode::IoError)?;
                count += 1;
                if count > 1024 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                let kind = entry.file_type().map_err(|_| ErrorCode::IoError)?;
                if kind.is_symlink() {
                    return Err(ErrorCode::Denied);
                }
                let path = entry.path();
                if kind.is_dir() {
                    if self.kind == ToolKind::YtDlp
                        && (depth != 0 || entry.file_name() == "_internal")
                    {
                        if depth >= 8 {
                            return Err(ErrorCode::QuotaExceeded);
                        }
                        super::super::files::identity::path_without_links(&path)?;
                        directories.push((path, depth + 1));
                    } else if depth == 0 && is_forbidden_loader_dir(&entry.file_name()) {
                        return Err(ErrorCode::Denied);
                    }
                } else {
                    if depth == 0 && is_forbidden_companion(&path) {
                        return Err(ErrorCode::Denied);
                    }
                    let native = is_native_component(&path);
                    if (native || depth > 0) && !approved.contains(&path) {
                        return Err(ErrorCode::Denied);
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn command(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new(&self.executable.path);
        cmd.current_dir(cwd).env_clear();
        for key in [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "TMP",
            "TEMP",
            "TMPDIR",
        ] {
            cmd.env(key, cwd);
        }
        cmd.env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONNOUSERSITE", "1");
        let path_env = if cfg!(unix) && self.kind == ToolKind::YtDlp {
            "/usr/bin:/bin"
        } else {
            ""
        };
        cmd.env("PATH", path_env)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("NO_COLOR", "1");
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", root);
        }
        cmd
    }
}
pub(super) fn is_native_component(path: &Path) -> bool {
    let ext_match = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "dll" | "so" | "dylib" | "pyd"
        )
    });
    let versioned_so = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().contains(".so."));
    ext_match || versioned_so
}
pub(super) fn is_forbidden_companion(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".manifest") || lower.ends_with(".local")
        })
}
pub(super) fn is_forbidden_loader_dir(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| {
        let lower = s.to_ascii_lowercase();
        lower.ends_with(".local") || matches!(lower.as_str(), "frameworks" | "lib" | "plugins")
    })
}
pub struct TrustedModel {
    file: PinnedFile,
    #[cfg_attr(windows, allow(dead_code))]
    digest: [u8; 32],
}
impl TrustedModel {
    pub fn approve(
        path: &Path,
        digest: [u8; 32],
        cancel: &HttpCancel,
    ) -> Result<Arc<Self>, ErrorCode> {
        let file = PinnedFile::open(path)?;
        if file.initial.size > 16u64 << 30 || file.sha256(cancel)? != digest {
            return Err(ErrorCode::Denied);
        }
        Ok(Arc::new(Self { file, digest }))
    }
    pub(super) fn path(&self, cancel: &HttpCancel) -> Result<&Path, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        #[cfg(windows)]
        {
            self.file.unchanged()?;
        }
        #[cfg(not(windows))]
        if self.file.sha256(cancel)? != self.digest {
            return Err(ErrorCode::StaleHandle);
        }
        if PinnedFile::open(&self.file.path)?.initial != self.file.initial {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(&self.file.path)
    }
}
