//! Local, same-user control for debug builds and explicit `devtools` builds.
//!
//! `IRIS_CONTROL` names a Unix socket on macOS, or `\\.\pipe\iris-...` on Windows.
//! Each UTF-8 line is one command (maximum 16 KiB including its newline):
//! `key <keystroke>`, `move <x> <y>`, `click <x> <y>`, `snap <file.png>`,
//! `resize <w> <h>`, `open <path>`, `state`, `get <property>`,
//! `mpv <command...>`, or `quit`. Replies remain on Iris's stderr.
//! Ordinary release builds ignore `IRIS_CONTROL` entirely.

use futures::channel::mpsc::UnboundedReceiver;

pub fn control_channel() -> Option<UnboundedReceiver<String>> {
    #[cfg(all(
        any(debug_assertions, feature = "devtools"),
        any(target_os = "macos", target_os = "windows")
    ))]
    {
        enabled::control_channel()
    }
    #[cfg(not(all(
        any(debug_assertions, feature = "devtools"),
        any(target_os = "macos", target_os = "windows")
    )))]
    {
        None
    }
}

#[cfg(all(
    any(debug_assertions, feature = "devtools"),
    any(target_os = "macos", target_os = "windows")
))]
mod enabled {
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use std::io::{self, BufRead, BufReader, Read};
    use std::time::Duration;

    const MAX_LINE: usize = 16 * 1024;
    const MAX_PENDING: usize = 256;

    pub(super) fn control_channel() -> Option<UnboundedReceiver<String>> {
        let path = std::env::var_os("IRIS_CONTROL")?;
        let listener = match platform::Listener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("control endpoint {path:?}: {error}");
                return None;
            }
        };
        let (tx, rx) = unbounded();
        match std::thread::Builder::new()
            .name("iris-control".into())
            .spawn(move || listener.run(tx))
        {
            Ok(_) => {
                log::info!("control listening on {path:?}");
                Some(rx)
            }
            Err(error) => {
                log::error!("starting control listener: {error}");
                None
            }
        }
    }

    // fill_buf bounds allocation even when a peer never sends a newline. Backpressure
    // bounds the otherwise-unbounded UI channel; only this thread owns its sender.
    fn forward(reader: impl Read, tx: &UnboundedSender<String>) -> io::Result<()> {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(()); // An incomplete final line is not a command.
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let count = end.map_or(available.len(), |index| index + 1);
            if bytes.len() + count > MAX_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "control line exceeds 16 KiB"));
            }
            bytes.extend_from_slice(&available[..count]);
            reader.consume(count);
            if end.is_none() {
                continue;
            }
            let mut line = String::from_utf8(std::mem::take(&mut bytes))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            line.truncate(line.trim_end().len());
            let start = line.len() - line.trim_start().len();
            line.drain(..start);
            if line.is_empty() {
                bytes = line.into_bytes();
                continue;
            }
            while tx.len() >= MAX_PENDING {
                if tx.is_closed() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if tx.unbounded_send(line).is_err() {
                return Ok(());
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod platform {
        use super::*;
        use std::ffi::OsStr;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;

        pub(super) struct Listener {
            socket: UnixListener,
            path: PathBuf,
            inode: u64,
            device: u64,
        }

        impl Listener {
            pub(super) fn bind(path: &OsStr) -> io::Result<Self> {
                // Never unlink an existing endpoint: it may belong to another instance.
                let socket = UnixListener::bind(path)?;
                let metadata = std::fs::symlink_metadata(path)?;
                let listener = Self {
                    socket,
                    path: path.into(),
                    inode: metadata.ino(),
                    device: metadata.dev(),
                };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                Ok(listener)
            }

            pub(super) fn run(self, tx: UnboundedSender<String>) {
                while !tx.is_closed() {
                    let (stream, _) = match self.socket.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            log::error!("accepting control connection: {error}");
                            return;
                        }
                    };
                    let mut uid = 0;
                    let mut gid = 0;
                    // SAFETY: stream owns a live socket and both output pointers are valid.
                    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
                    // Checking the peer also closes the bind-to-chmod permission window.
                    if result != 0 || uid != unsafe { libc::geteuid() } {
                        log::warn!("rejecting control connection from a different or unknown user");
                        continue;
                    }
                    if let Err(error) = forward(stream, &tx) {
                        log::warn!("control connection closed: {error}");
                    }
                }
            }
        }

        impl Drop for Listener {
            fn drop(&mut self) {
                if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
                    if metadata.ino() == self.inode && metadata.dev() == self.device {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    mod platform {
        use super::*;
        use std::ffi::{OsStr, c_void};
        use std::fs::File;
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        };
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // Conversion APIs allocate with LocalAlloc, not Rust's allocator.
        struct LocalAllocation(*mut c_void);

        impl Drop for LocalAllocation {
            fn drop(&mut self) {
                unsafe { LocalFree(self.0); }
            }
        }

        pub(super) struct Listener(File);

        impl Listener {
            pub(super) fn bind(path: &OsStr) -> io::Result<Self> {
                let name: Vec<u16> = path.encode_wide().collect();
                let prefix: Vec<u16> = OsStr::new(r"\\.\pipe\iris-").encode_wide().collect();
                if !name.starts_with(&prefix) || name.len() == prefix.len()
                    || name[prefix.len()..].iter().any(|ch| matches!(*ch, 0 | 47 | 92))
                {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput,
                        r"IRIS_CONTROL must be \\.\pipe\iris-<unique-name>"));
                }
                let name: Vec<u16> = name.into_iter().chain(Some(0)).collect();
                // SAFETY: all Win32 buffers are owned, aligned, and remain live through
                // their calls; successful OS handles/allocations immediately gain owners.
                unsafe {
                    let mut token = std::ptr::null_mut();
                    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let token = OwnedHandle::from_raw_handle(token);
                    let mut needed = 0;
                    GetTokenInformation(token.as_raw_handle(), TokenUser, std::ptr::null_mut(), 0, &mut needed);
                    if needed == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let mut info = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
                    if GetTokenInformation(token.as_raw_handle(), TokenUser, info.as_mut_ptr().cast(),
                        needed, &mut needed) == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let user = &*info.as_ptr().cast::<TOKEN_USER>();
                    let mut sid = std::ptr::null_mut();
                    if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let _sid_owner = LocalAllocation(sid.cast());
                    let mut length = 0;
                    while *sid.add(length) != 0 { length += 1; }
                    let sid = String::from_utf16(std::slice::from_raw_parts(sid, length))
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    // Protected DACL: no inherited grants, only the current token's user.
                    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})").encode_utf16().chain(Some(0)).collect();
                    let mut descriptor = std::ptr::null_mut();
                    if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(),
                        SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let _descriptor_owner = LocalAllocation(descriptor);
                    let security = SECURITY_ATTRIBUTES {
                        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                        lpSecurityDescriptor: descriptor,
                        bInheritHandle: 0,
                    };
                    let pipe = CreateNamedPipeW(name.as_ptr(),
                        PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                        1, 0, MAX_LINE as u32, 0, &security);
                    if pipe == INVALID_HANDLE_VALUE {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(Self(File::from_raw_handle(pipe)))
                }
            }

            pub(super) fn run(mut self, tx: UnboundedSender<String>) {
                while !tx.is_closed() {
                    // Blocking accept/read prevent a disconnected client from hot-spinning.
                    let connected = unsafe { ConnectNamedPipe(self.0.as_raw_handle(), std::ptr::null_mut()) };
                    if connected == 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                            log::error!("accepting control pipe: {error}");
                            return;
                        }
                    }
                    if let Err(error) = forward(&mut self.0, &tx) {
                        log::warn!("control connection closed: {error}");
                    }
                    if unsafe { DisconnectNamedPipe(self.0.as_raw_handle()) } == 0 {
                        log::error!("disconnecting control pipe: {}", io::Error::last_os_error());
                        return;
                    }
                }
            }
        }
    }
}
