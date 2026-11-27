//! Host-selected local file identities. Guest values never become filesystem paths.
use iris_api::v2::ErrorCode;
use iris_base::net::http_client::HttpCancel;
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId(pub u64, pub u64);
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub id: FileId,
    pub size: u64,
    modified: SystemTime,
    changed: u64,
}
pub(crate) fn stamp(file: &File) -> Result<Stamp, ErrorCode> {
    let meta = file.metadata().map_err(|_| ErrorCode::IoError)?;
    if !meta.is_file() {
        return Err(ErrorCode::Denied);
    }
    let (id, links, changed) = platform(file, &meta)?;
    // Input hardlinks would otherwise obscure source/output alias checks.
    if links != 1 {
        return Err(ErrorCode::Denied);
    }
    Ok(Stamp {
        id,
        size: meta.len(),
        modified: meta.modified().map_err(|_| ErrorCode::IoError)?,
        changed,
    })
}
#[cfg(windows)]
fn allowed_windows_reparse_tag(tag: u32) -> bool {
    matches!(
        tag,
        0x9000_001A
            ..=0x9000_001F // IO_REPARSE_TAG_CLOUD .. IO_REPARSE_TAG_CLOUD_F (OneDrive)
        | 0x8000_0021             // IO_REPARSE_TAG_ONEDRIVE
        | 0x8000_0013             // IO_REPARSE_TAG_DEDUP
        | 0x8000_0017 // IO_REPARSE_TAG_WOF
    )
}
#[cfg(windows)]
fn windows_reparse_tag(file: &File) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo, GetFileInformationByHandleEx,
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    (ok != 0).then_some(info.ReparseTag)
}
#[cfg(windows)]
fn windows_path_allows_reparse(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let Ok(file) = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    else {
        return false;
    };
    windows_reparse_tag(&file).is_some_and(allowed_windows_reparse_tag)
}
#[cfg(windows)]
fn platform(file: &File, meta: &Metadata) -> Result<(FileId, u64, u64), ErrorCode> {
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && !windows_reparse_tag(file).is_some_and(|t| t == 0 || allowed_windows_reparse_tag(t))
    {
        return Err(ErrorCode::Denied);
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file remains open and info is an initialized output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(ErrorCode::IoError);
    }
    Ok((
        FileId(
            info.dwVolumeSerialNumber as u64,
            (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64,
        ),
        info.nNumberOfLinks as u64,
        meta.creation_time(),
    ))
}
#[cfg(unix)]
fn platform(_file: &File, meta: &Metadata) -> Result<(FileId, u64, u64), ErrorCode> {
    use std::os::unix::fs::MetadataExt;
    Ok((
        FileId(meta.dev(), meta.ino()),
        meta.nlink(),
        (meta.ctime() as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(meta.ctime_nsec() as u64),
    ))
}
pub(crate) fn path_without_links(path: &Path) -> Result<PathBuf, ErrorCode> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(ErrorCode::Denied);
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        if matches!(part, Component::ParentDir | Component::CurDir) {
            return Err(ErrorCode::Denied);
        }
        current.push(part);
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        let metadata = current.symlink_metadata().map_err(|_| ErrorCode::IoError)?;
        if metadata.is_symlink() {
            #[cfg(target_os = "macos")]
            if matches!(current.to_str(), Some("/var" | "/tmp" | "/etc")) {
                continue;
            }
            return Err(ErrorCode::Denied);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 && !windows_path_allows_reparse(&current) {
                return Err(ErrorCode::Denied);
            }
        }
    }
    path.canonicalize().map_err(|_| ErrorCode::IoError)
}
pub(crate) fn canonical_host_dir(path: &Path) -> Result<PathBuf, ErrorCode> {
    let canonical = path.canonicalize().map_err(|_| ErrorCode::IoError)?;
    path_without_links(&canonical)
}
pub(crate) struct PinnedFile {
    pub initial: Stamp,
    file: Mutex<File>,
}
impl PinnedFile {
    /// Blocking host operation: only after user/current-media authorization.
    pub(crate) fn open(path: &Path) -> Result<Self, ErrorCode> {
        let path = path_without_links(path)?;
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            // Open with FILE_SHARE_READ (no write/delete sharing) and OPEN_REPARSE_POINT first
            // so no concurrent rename/symlink swap can race between inspection and pinning.
            let probe = OpenOptions::new()
                .read(true)
                .share_mode(1)
                .custom_flags(0x0020_0000)
                .open(&path)
                .map_err(|_| ErrorCode::IoError)?;
            let probe_meta = probe.metadata().map_err(|_| ErrorCode::IoError)?;
            if probe_meta.file_attributes() & 0x400 == 0 {
                probe
            } else {
                if !windows_reparse_tag(&probe).is_some_and(allowed_windows_reparse_tag) {
                    return Err(ErrorCode::Denied);
                }
                let probe_stamp = stamp(&probe)?;
                let hydrated = OpenOptions::new()
                    .read(true)
                    .share_mode(1)
                    .open(&path)
                    .map_err(|_| ErrorCode::IoError)?;
                if stamp(&hydrated)?.id != probe_stamp.id {
                    return Err(ErrorCode::Denied);
                }
                drop(probe);
                hydrated
            }
        };
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .map_err(|_| ErrorCode::IoError)?
        };
        let initial = stamp(&file)?;
        Ok(Self {
            initial,
            file: Mutex::new(file),
        })
    }
    pub(crate) fn unchanged(&self) -> Result<(), ErrorCode> {
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if stamp(&file)? != self.initial {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(())
    }
    pub(crate) fn read_at(
        &self,
        offset: u64,
        bytes: &mut [u8],
        cancel: &HttpCancel,
    ) -> Result<usize, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if stamp(&file)? != self.initial {
            return Err(ErrorCode::StaleHandle);
        }
        if offset > self.initial.size {
            return Err(ErrorCode::RangeInvalid);
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| ErrorCode::IoError)?;
        let read = file.read(bytes).map_err(|_| ErrorCode::IoError)?;
        if stamp(&file)? != self.initial {
            return Err(ErrorCode::StaleHandle);
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(read)
    }
}
