//! Local, same-user control for debug builds and explicit `devtools` builds.
//!
//! `IRIS_CONTROL` names a Unix socket on macOS, or `\\.\pipe\iris-...` on Windows.
//! Each UTF-8 line is one command (maximum 16 KiB including its newline):
//! `key <keystroke>`, `move <x> <y>`, `click <x> <y>`, `snap <file.png>`,
//! `resize <w> <h>`, `open <path>`, `filter <all|video|audio|photo|favorites>`,
//! `state`, `ui`, `get <property>`, `mpv <command...>`, `sim_device_reset`, or `quit`.
//! Replies remain on Iris's stderr.
//! Ordinary release builds ignore `IRIS_CONTROL` entirely.

#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) fn control_channel() -> Option<futures::channel::mpsc::UnboundedReceiver<String>> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        enabled::control_channel()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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

    pub(super) const MAX_LINE: usize = 16 * 1024;
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
        match iris_base::rt::spawn_thread(iris_base::rt::ThreadRole::ControlPipe, move || {
            listener.run(tx)
        }) {
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
    pub(super) fn forward(reader: impl Read, tx: &UnboundedSender<String>) -> io::Result<()> {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let available = match reader.fill_buf() {
                Ok(buf) => buf,
                Err(e)
                    if e.kind() == io::ErrorKind::BrokenPipe
                        || e.raw_os_error() == Some(232)
                        || e.raw_os_error() == Some(109) =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(()); // An incomplete final line is not a command.
            }
            let end = available.iter().position(|byte| *byte == b'\n');
            let count = end.map_or(available.len(), |index| index + 1);
            if bytes.len() + count > MAX_LINE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control line exceeds 16 KiB",
                ));
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
        use super::forward;
        use futures::channel::mpsc::UnboundedSender;
        use std::ffi::OsStr;
        use std::io;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::sync::{Mutex, Once};

        static ATEXIT_SOCKET: Mutex<Option<(PathBuf, u64, u64)>> = Mutex::new(None);
        static ATEXIT_INIT: Once = Once::new();

        fn unlink_if_same(path: &PathBuf, inode: u64, device: u64) {
            if let Ok(metadata) = std::fs::symlink_metadata(path)
                && metadata.ino() == inode
                && metadata.dev() == device
            {
                let _ = std::fs::remove_file(path);
            }
        }

        extern "C" fn cleanup_atexit() {
            if let Ok(mut guard) = ATEXIT_SOCKET.lock()
                && let Some((path, inode, device)) = guard.take()
            {
                unlink_if_same(&path, inode, device);
            }
        }

        pub(super) struct Listener {
            socket: UnixListener,
            path: PathBuf,
            inode: u64,
            device: u64,
        }

        impl Listener {
            pub(super) fn bind(path: &OsStr) -> io::Result<Self> {
                let socket = UnixListener::bind(path)?;
                let metadata = std::fs::symlink_metadata(path)?;
                let path_buf = PathBuf::from(path);
                let (inode, device) = (metadata.ino(), metadata.dev());
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                *ATEXIT_SOCKET.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((path_buf.clone(), inode, device));
                ATEXIT_INIT.call_once(|| unsafe {
                    let _ = libc::atexit(cleanup_atexit);
                });
                Ok(Self {
                    socket,
                    path: path_buf,
                    inode,
                    device,
                })
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
                    let result =
                        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
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
                let _ = ATEXIT_SOCKET
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                unlink_if_same(&self.path, self.inode, self.device);
            }
        }
    }

    #[cfg(target_os = "windows")]
    mod platform {
        use super::{MAX_LINE, forward};
        use futures::channel::mpsc::UnboundedSender;
        use std::ffi::OsStr;
        use std::fs::File;
        use std::io;
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        };
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};

        pub(super) struct Listener(File);

        impl Listener {
            pub(super) fn bind(path: &OsStr) -> io::Result<Self> {
                let name: Vec<u16> = path.encode_wide().collect();
                let prefix: Vec<u16> = OsStr::new(r"\\.\pipe\iris-").encode_wide().collect();
                if !name.starts_with(&prefix)
                    || name.len() == prefix.len()
                    || name[prefix.len()..]
                        .iter()
                        .any(|ch| matches!(*ch, 0 | 47 | 92))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        r"IRIS_CONTROL must be \\.\pipe\iris-<unique-name>",
                    ));
                }
                let name: Vec<u16> = name.into_iter().chain(Some(0)).collect();
                crate::platform::pipe::create_inbound(&name, MAX_LINE as u32).map(Self)
            }

            pub(super) fn run(mut self, tx: UnboundedSender<String>) {
                while !tx.is_closed() {
                    let connected =
                        unsafe { ConnectNamedPipe(self.0.as_raw_handle(), std::ptr::null_mut()) };
                    if connected == 0 {
                        let error = io::Error::last_os_error();
                        match error.raw_os_error() {
                            Some(code) if code == ERROR_PIPE_CONNECTED as i32 => {}
                            Some(code)
                                if code == ERROR_NO_DATA as i32
                                    || code == ERROR_BROKEN_PIPE as i32 =>
                            {
                                let _ = unsafe { DisconnectNamedPipe(self.0.as_raw_handle()) };
                                continue;
                            }
                            _ => {
                                log::error!("accepting control pipe: {error}");
                                return;
                            }
                        }
                    }
                    if let Err(error) = forward(&mut self.0, &tx) {
                        log::warn!("control connection closed: {error}");
                    }
                    if unsafe { DisconnectNamedPipe(self.0.as_raw_handle()) } == 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(ERROR_NO_DATA as i32)
                            && error.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32)
                        {
                            log::error!("disconnecting control pipe: {error}");
                            return;
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{MAX_LINE, forward};
        use futures::channel::mpsc::unbounded;
        use std::io::{self, Cursor};

        #[test]
        fn forward_trims_skips_blank_and_drops_incomplete_tail() {
            let (tx, mut rx) = unbounded();
            let payload = b"  state \n\n\t\r\n  key space \r\nincomplete_tail";
            forward(Cursor::new(payload), &tx).unwrap();

            let mut received = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                received.push(msg);
            }
            assert_eq!(received, vec!["state", "key space"]);
        }

        #[test]
        fn forward_enforces_16kib_line_cap_and_rejects_bad_utf8() {
            let (tx, mut rx) = unbounded();
            let mut exact = vec![b'a'; MAX_LINE - 1];
            exact.push(b'\n');
            forward(Cursor::new(exact), &tx).unwrap();
            assert_eq!(rx.try_recv().unwrap().len(), MAX_LINE - 1);

            let mut too_long = vec![b'x'; MAX_LINE];
            too_long.push(b'\n');
            let err = forward(Cursor::new(too_long), &tx).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);

            let err_utf8 = forward(Cursor::new(b"\xFF\xFE\n"), &tx).unwrap_err();
            assert_eq!(err_utf8.kind(), io::ErrorKind::InvalidData);

            drop(rx);
            forward(Cursor::new(b"quit\n"), &tx).unwrap();
        }
    }
}
