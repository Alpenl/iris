//! Windows file associations start a new process per file; forward the path
//! to the running instance of this user and logon session instead. Only a
//! bounded file path crosses the pipe, never a playback option or command.
//! macOS needs none of this: LaunchServices delivers opens to the running app.

#[cfg(target_os = "windows")]
mod imp {
    use futures::channel::mpsc::UnboundedSender;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GetLastError,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, DisconnectNamedPipe, GetNamedPipeServerSessionId,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};

    /// Longest Windows path in UTF-16 units (the `\\?\` limit).
    const MAX_UNITS: usize = 32_767;

    /// Holds the per-session mutex for the life of the primary instance.
    pub struct Guard(#[allow(dead_code)] Option<OwnedHandle>);

    pub enum Claim {
        /// This process is the primary instance; keep the guard alive.
        Primary(Guard),
        /// The path went to the running instance; this process should exit.
        Forwarded,
    }

    fn session_id() -> io::Result<u32> {
        let mut session = 0;
        // SAFETY: plain out-parameter call.
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(session)
    }

    /// Becomes the primary instance or hands `initial` to the existing one.
    /// Files later sent by other processes arrive on `opened`. Isolated test
    /// instances (`IRIS_SETTINGS`) never take part.
    pub fn claim(initial: Option<&Path>, opened: UnboundedSender<PathBuf>) -> Claim {
        if std::env::var_os("IRIS_SETTINGS").is_some() {
            return Claim::Primary(Guard(None));
        }
        let (session, sid) = match (session_id(), crate::platform::pipe::current_user_sid()) {
            (Ok(session), Ok(sid)) => (session, sid),
            (Err(e), _) | (_, Err(e)) => {
                log::error!("single instance: {e}");
                return Claim::Primary(Guard(None));
            }
        };
        let endpoint = format!(r"\\.\pipe\iris-open-{session}-{sid}");
        let name: Vec<u16> = r"Local\Iris.Player".encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated name; a non-null handle immediately gains an owner.
        let (mutex, exists) = unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            let exists = GetLastError() == ERROR_ALREADY_EXISTS;
            (
                (!handle.is_null()).then(|| OwnedHandle::from_raw_handle(handle)),
                exists,
            )
        };
        if exists {
            match forward(&endpoint, session, initial) {
                Ok(()) => return Claim::Forwarded,
                Err(e) => log::error!("forwarding to the running instance: {e}; starting another"),
            }
            return Claim::Primary(Guard(mutex));
        }
        let wide: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
        match crate::platform::pipe::create_inbound(&wide, (MAX_UNITS * 2 + 4) as u32) {
            Ok(pipe) => {
                if let Err(e) = std::thread::Builder::new()
                    .name("iris-open-files".into())
                    .spawn(move || serve(pipe, opened))
                {
                    log::error!("starting file-open listener: {e}");
                }
            }
            Err(e) => log::error!("file-open pipe: {e}"),
        }
        Claim::Primary(Guard(mutex))
    }

    fn forward(endpoint: &str, session: u32, path: Option<&Path>) -> io::Result<()> {
        // The primary has its own working directory; send an absolute path.
        let path = path.map(std::path::absolute).transpose()?;
        let units: Vec<u16> = path
            .map(|p| p.as_os_str().encode_wide().collect())
            .unwrap_or_default();
        if units.len() > MAX_UNITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file path is too long",
            ));
        }
        // The primary may still be starting its listener; give it a moment.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut pipe = loop {
            match OpenOptions::new().write(true).open(endpoint) {
                Ok(pipe) => break pipe,
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        // Pipe names are global; refuse a server outside this logon session.
        let mut server = u32::MAX;
        // SAFETY: valid pipe handle and out-parameter.
        if unsafe { GetNamedPipeServerSessionId(pipe.as_raw_handle(), &mut server) } == 0
            || server != session
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pipe server is in another session",
            ));
        }
        let mut message = Vec::with_capacity(4 + units.len() * 2);
        message.extend_from_slice(&(units.len() as u32).to_le_bytes());
        units
            .iter()
            .for_each(|unit| message.extend_from_slice(&unit.to_le_bytes()));
        pipe.write_all(&message)
    }

    fn serve(mut pipe: std::fs::File, opened: UnboundedSender<PathBuf>) {
        while !opened.is_closed() {
            // SAFETY: the pipe handle is owned by `pipe` for the whole loop.
            if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    log::error!("accepting file-open pipe: {error}");
                    return;
                }
            }
            match read_path(&mut pipe) {
                // An empty path only asks the running window to come forward.
                Ok(path) => {
                    let _ = opened.unbounded_send(path);
                }
                Err(e) => log::warn!("file-open message: {e}"),
            }
            if unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) } == 0 {
                log::error!(
                    "disconnecting file-open pipe: {}",
                    io::Error::last_os_error()
                );
                return;
            }
        }
    }

    fn read_path(reader: &mut impl Read) -> io::Result<PathBuf> {
        let mut len = [0u8; 4];
        reader.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_UNITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file path is too long",
            ));
        }
        let mut bytes = vec![0u8; len * 2];
        reader.read_exact(&mut bytes)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        if units.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NUL in file path",
            ));
        }
        Ok(PathBuf::from(OsString::from_wide(&units)))
    }
}

#[cfg(target_os = "windows")]
pub use imp::{Claim, claim};
