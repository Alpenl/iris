use super::{Artifact, identity::canonical_host_dir};
use crate::{
    plugin_v2::ledger::{Ledger, Resource},
    vfs::HttpCancel,
};
use cap_std::fs::{Dir, OpenOptions};
use iris_api::v2::{ErrorCode, Generation, enhance::ExportFormat};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportState {
    Pending,
    Writing,
    Committed,
    Failed(ErrorCode),
}
struct Audit {
    state: Mutex<ExportState>,
    _lease: crate::plugin_v2::ledger::Reservation,
}
/// Retained by the host UI independently of guest completion/cancellation. Once
/// the atomic link succeeds it stays Committed, even if delivery is cancelled.
#[derive(Clone)]
pub struct ExportAudit(Arc<Audit>);
impl ExportAudit {
    pub fn state(&self) -> ExportState {
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set(&self, state: ExportState) {
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
}
struct AuditTicket(ExportAudit);
impl Drop for AuditTicket {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, ExportState::Pending | ExportState::Writing) {
            *state = ExportState::Failed(ErrorCode::Cancelled);
        }
    }
}

pub struct ExportGrant {
    owner: String,
    generation: Generation,
    dir: Dir,
    parent_path: PathBuf,
    filename: String,
    format: ExportFormat,
    expires: Instant,
    audit: AuditTicket,
    _lease: crate::plugin_v2::ledger::Reservation,
}
impl ExportGrant {
    /// The target is selected by the host save dialog or an explicitly approved
    /// default directory. This constructor is never callable by the guest.
    pub fn approve(
        path: &Path,
        format: ExportFormat,
        owner: String,
        generation: Generation,
        ledger: &Ledger,
    ) -> Result<Self, ErrorCode> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(ErrorCode::InvalidArgument)?;
        validate_name(filename, extension(format))?;
        let parent = canonical_host_dir(path.parent().ok_or(ErrorCode::InvalidArgument)?)?;
        let dir = Dir::open_ambient_dir(&parent, cap_std::ambient_authority())
            .map_err(|_| ErrorCode::IoError)?;
        if dir.symlink_metadata(filename).is_ok() {
            return Err(ErrorCode::Denied);
        }
        Ok(Self {
            owner: owner.clone(),
            generation,
            dir,
            parent_path: parent,
            filename: filename.into(),
            format,
            expires: Instant::now() + Duration::from_secs(300),
            audit: AuditTicket(ExportAudit(Arc::new(Audit {
                state: Mutex::new(ExportState::Pending),
                _lease: ledger
                    .reserve(&owner, &[(Resource::Handle, 1), (Resource::HostBytes, 512)])?,
            }))),
            _lease: ledger.reserve(
                &owner,
                &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
            )?,
        })
    }
    pub fn audit(&self) -> ExportAudit {
        self.audit.0.clone()
    }
    /// Background-only, single-use grant. A same-directory temporary file is
    /// linked into its final name atomically without replacing any existing name.
    pub fn commit(
        self,
        artifact: &Artifact,
        owner: &str,
        generation: Generation,
        ledger: &Ledger,
        cancel: &HttpCancel,
    ) -> Result<(), ErrorCode> {
        let audit = self.audit();
        let result = self.commit_inner(artifact, owner, generation, ledger, cancel);
        if let Err(error) = result {
            audit.set(ExportState::Failed(error));
        }
        result
    }
    fn commit_inner(
        self,
        artifact: &Artifact,
        owner: &str,
        generation: Generation,
        ledger: &Ledger,
        cancel: &HttpCancel,
    ) -> Result<(), ErrorCode> {
        if owner != self.owner || artifact.owner() != owner {
            return Err(ErrorCode::Denied);
        }
        if generation != self.generation || Instant::now() >= self.expires {
            return Err(ErrorCode::StaleHandle);
        }
        if artifact.format != extension(self.format) {
            return Err(ErrorCode::InvalidArgument);
        }
        self.audit.0.set(ExportState::Writing);
        let copy = ledger.reserve(
            owner,
            &[
                (Resource::TempBytes, artifact.size()),
                (Resource::HostBytes, 65536),
            ],
        )?;
        let name = format!(".{}.partial", super::random_name()?);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = self
                .dir
                .open_with(&name, &options)
                .map_err(|_| ErrorCode::IoError)?;
            use std::io::Write;
            let mut bytes = [0; 65536];
            let mut offset = 0;
            while offset < artifact.size() {
                let n = artifact.read_at(offset, &mut bytes, cancel)?;
                if n == 0 {
                    return Err(ErrorCode::IoError);
                }
                file.write_all(&bytes[..n])
                    .map_err(|_| ErrorCode::IoError)?;
                offset += n as u64;
            }
            file.sync_all().map_err(|_| ErrorCode::IoError)?;
            drop(file);
            if cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            link_or_noreplace_rename(&self.dir, &self.parent_path, &name, &self.filename)?;
            self.audit.0.set(ExportState::Committed);
            Ok(())
        })();
        if let Err(error) = result {
            self.audit.0.set(ExportState::Failed(error));
        }
        artifact.retire_export(self.dir, name, vec![copy, self._lease]);
        result
    }
}
pub fn extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Edl => "edl",
        ExportFormat::Xmp => "xmp",
        ExportFormat::Csv => "csv",
        ExportFormat::SubRip => "srt",
        ExportFormat::WebVtt => "vtt",
        ExportFormat::Ass => "ass",
        ExportFormat::Text => "txt",
        ExportFormat::Matroska => "mkv",
        ExportFormat::Mp4 => "mp4",
        ExportFormat::Webm => "webm",
    }
}
fn link_or_noreplace_rename(
    dir: &Dir,
    parent_path: &Path,
    from: &str,
    to: &str,
) -> Result<(), ErrorCode> {
    match dir.hard_link(from, dir, to) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(ErrorCode::Denied),
        Err(e) if hardlink_unsupported(&e) => {}
        Err(_) => return Err(ErrorCode::IoError),
    }
    noreplace_rename(dir, parent_path, from, to)
}
fn hardlink_unsupported(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(err.raw_os_error(), Some(1 | 50 | 87))
    }
    #[cfg(unix)]
    {
        matches!(
            err.raw_os_error(),
            Some(
                libc::ENOTSUP
                    | libc::EOPNOTSUPP
                    | libc::EXDEV
                    | libc::EPERM
                    | libc::ENOSYS
                    | libc::EINVAL
            )
        )
    }
}
#[cfg(windows)]
fn noreplace_rename(_dir: &Dir, parent_path: &Path, from: &str, to: &str) -> Result<(), ErrorCode> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS},
        Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FileRenameInfo, SetFileInformationByHandle,
        },
    };
    #[repr(C)]
    struct RenameHeader {
        flags: usize,
        root_directory: *mut std::ffi::c_void,
        file_name_length: u32,
    }
    let handle = std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent_path.join(from))
        .map_err(|_| ErrorCode::IoError)?;
    let target_wide: Vec<u16> = std::ffi::OsStr::new(to).encode_wide().collect();
    let name_bytes = target_wide
        .len()
        .checked_mul(2)
        .ok_or(ErrorCode::InvalidArgument)?;
    let total_bytes = size_of::<RenameHeader>()
        .checked_add(name_bytes)
        .ok_or(ErrorCode::InvalidArgument)?;
    let words = total_bytes.div_ceil(size_of::<usize>());
    let mut buf = vec![0usize; words];
    let header = RenameHeader {
        flags: 0, // ReplaceIfExists = FALSE
        root_directory: std::ptr::null_mut(),
        file_name_length: name_bytes as u32,
    };
    unsafe {
        std::ptr::write(buf.as_mut_ptr().cast::<RenameHeader>(), header);
        std::ptr::copy_nonoverlapping(
            target_wide.as_ptr(),
            buf.as_mut_ptr()
                .cast::<u8>()
                .add(size_of::<RenameHeader>())
                .cast::<u16>(),
            target_wide.len(),
        );
    }
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileRenameInfo,
            buf.as_ptr().cast(),
            total_bytes as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let raw = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
    if matches!(raw, ERROR_ALREADY_EXISTS | ERROR_ACCESS_DENIED) {
        Err(ErrorCode::Denied)
    } else {
        Err(ErrorCode::IoError)
    }
}
#[cfg(target_os = "macos")]
fn noreplace_rename(dir: &Dir, _parent_path: &Path, from: &str, to: &str) -> Result<(), ErrorCode> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let from_c = CString::new(from).map_err(|_| ErrorCode::InvalidArgument)?;
    let to_c = CString::new(to).map_err(|_| ErrorCode::InvalidArgument)?;
    let rc = unsafe {
        libc::renameatx_np(
            dir.as_raw_fd(),
            from_c.as_ptr(),
            dir.as_raw_fd(),
            to_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
        Err(ErrorCode::Denied)
    } else {
        Err(ErrorCode::IoError)
    }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn noreplace_rename(dir: &Dir, _parent_path: &Path, from: &str, to: &str) -> Result<(), ErrorCode> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let from_c = CString::new(from).map_err(|_| ErrorCode::InvalidArgument)?;
    let to_c = CString::new(to).map_err(|_| ErrorCode::InvalidArgument)?;
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                dir.as_raw_fd(),
                from_c.as_ptr(),
                dir.as_raw_fd(),
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists {
            return Err(ErrorCode::Denied);
        }
    }
    let _ = (dir, from_c, to_c);
    Err(ErrorCode::IoError)
}
fn validate_name(name: &str, ext: &str) -> Result<(), ErrorCode> {
    if name.len() > 240
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| {
            c.is_control()
                || ":/\\<>\"|?*".contains(c)
                || matches!(
                    c,
                    '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{061C}'
                        | '\u{200E}'
                        | '\u{200F}'
                )
        })
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let com_or_lpt = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(
            &stem[3..],
            "0" | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
                | "9"
                | "\u{00B9}"
                | "\u{00B2}"
                | "\u{00B3}"
        );
    if stem.is_empty()
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || com_or_lpt
        || !Path::new(name)
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case(ext))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
