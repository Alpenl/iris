use super::super::binding::{
    ToolApproval, is_forbidden_companion, is_forbidden_loader_dir, is_native_component,
};
use super::*;
use crate::plugin_v2::files::identity::{PinnedFile, path_without_links};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "tool",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Selection {
    Tool(ToolKind),
    WhisperModel,
}
impl Selection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tool(ToolKind::Ffmpeg) => "FFmpeg 8.0.1",
            Self::Tool(ToolKind::Ffprobe) => "FFprobe 8.0.1",
            Self::Tool(ToolKind::WhisperCpp) => "whisper.cpp 1.8.2",
            Self::Tool(ToolKind::YtDlp) => "yt-dlp 2025.11.12（目录版）",
            Self::WhisperModel => "Whisper 模型",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub selection: Selection,
    pub path: PathBuf,
    pub sha256: [u8; 32],
    pub components: Vec<(PathBuf, [u8; 32])>,
}
pub(super) enum Approved {
    Tool(Arc<TrustedTool>),
    Model(Arc<TrustedModel>),
}
impl Receipt {
    /// Trusted file picker supplies path. No version command or native code runs.
    pub fn inspect(
        selection: Selection,
        path: &Path,
        cancel: &HttpCancel,
    ) -> Result<Self, ErrorCode> {
        #[cfg(unix)]
        let resolved = path.canonicalize().map_err(|_| ErrorCode::IoError)?;
        #[cfg(unix)]
        let path = resolved.as_path();
        let file = PinnedFile::open(path)?;
        let limit = if selection == Selection::WhisperModel {
            16u64 << 30
        } else {
            512 << 20
        };
        if file.initial.size > limit {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut this = Self {
            selection,
            path: file.path.clone(),
            sha256: file.sha256(cancel)?,
            components: Vec::new(),
        };
        if let Selection::Tool(kind) = selection {
            let parent = file.path.parent().ok_or(ErrorCode::Denied)?;
            let mut dirs = vec![(parent.to_owned(), 0)];
            let mut count = 0;
            let mut bytes = 0u64;
            while let Some((dir, depth)) = dirs.pop() {
                for entry in std::fs::read_dir(dir).map_err(|_| ErrorCode::IoError)? {
                    let entry = entry.map_err(|_| ErrorCode::IoError)?;
                    count += 1;
                    if count > 1024 {
                        return Err(ErrorCode::QuotaExceeded);
                    }
                    let kind_file = entry.file_type().map_err(|_| ErrorCode::IoError)?;
                    let path = entry.path();
                    if kind_file.is_symlink() {
                        return Err(ErrorCode::Denied);
                    }
                    if kind_file.is_dir() {
                        if kind == ToolKind::YtDlp
                            && (depth > 0 || entry.file_name() == "_internal")
                        {
                            if depth >= 8 {
                                return Err(ErrorCode::QuotaExceeded);
                            }
                            path_without_links(&path)?;
                            dirs.push((path, depth + 1));
                        } else if depth == 0 && is_forbidden_loader_dir(&entry.file_name()) {
                            return Err(ErrorCode::Denied);
                        }
                    } else {
                        if depth == 0 && is_forbidden_companion(&path) {
                            return Err(ErrorCode::Denied);
                        }
                        if depth > 0 || is_native_component(&path) {
                            if this.components.len() >= 256 {
                                return Err(ErrorCode::QuotaExceeded);
                            }
                            let component = PinnedFile::open(&path)?;
                            bytes = bytes
                                .checked_add(component.initial.size)
                                .ok_or(ErrorCode::QuotaExceeded)?;
                            if component.initial.size > 2 << 30 || bytes > 4 << 30 {
                                return Err(ErrorCode::QuotaExceeded);
                            }
                            this.components
                                .push((component.path.clone(), component.sha256(cancel)?));
                        }
                    }
                }
            }
            this.components.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Ok(this)
    }
    pub(super) fn approve(&self, cwd: &Path, cancel: &HttpCancel) -> Result<Approved, ErrorCode> {
        if !self.path.is_absolute()
            || self.components.len() > 256
            || self.components.iter().any(|(p, _)| !p.is_absolute())
        {
            return Err(ErrorCode::Denied);
        }
        match self.selection {
            Selection::Tool(kind) => TrustedTool::approve(
                &self.path,
                ToolApproval {
                    kind,
                    version: kind.supported_version().into(),
                    executable_sha256: self.sha256,
                    components: self.components.clone(),
                },
                cwd,
                cancel,
            )
            .map(Approved::Tool),
            Selection::WhisperModel if self.components.is_empty() => {
                TrustedModel::approve(&self.path, self.sha256, cancel).map(Approved::Model)
            }
            _ => Err(ErrorCode::InvalidArgument),
        }
    }
}
