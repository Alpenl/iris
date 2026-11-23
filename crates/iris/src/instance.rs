//! Windows file associations start a new process per file; forward the path
//! to the running instance of this user and logon session instead. Only a
//! bounded file path crosses the pipe, never a playback option or command.
//! macOS needs none of this: LaunchServices delivers opens to the running app.

#[cfg(any(target_os = "windows", test))]
mod wire {
    use std::io::{self, Read};

    /// Longest Windows path in UTF-16 units (the `\\?\` limit).
    pub(super) const MAX_UNITS: usize = 32_767;

    /// Reads a length-prefixed UTF-16LE path payload with strict size and NUL checks.
    pub(super) fn read_path_units(reader: &mut impl Read) -> io::Result<Vec<u16>> {
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
        Ok(units)
    }
}

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

    use super::wire::{MAX_UNITS, read_path_units};

    /// Holds the per-session mutex for the life of the primary instance.
    pub(crate) struct Guard {
        _handle: Option<OwnedHandle>,
    }

    pub(crate) enum Claim {
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
    pub(crate) fn claim(initial: Option<&Path>, opened: UnboundedSender<PathBuf>) -> Claim {
        if std::env::var_os("IRIS_SETTINGS").is_some() {
            return Claim::Primary(Guard { _handle: None });
        }
        let (session, sid) = match (session_id(), crate::platform::pipe::current_user_sid()) {
            (Ok(session), Ok(sid)) => (session, sid),
            (Err(e), _) | (_, Err(e)) => {
                log::error!("single instance: {e}");
                return Claim::Primary(Guard { _handle: None });
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
            return Claim::Primary(Guard { _handle: mutex });
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
        Claim::Primary(Guard { _handle: mutex })
    }

    fn forward(endpoint: &str, session: u32, path: Option<&Path>) -> io::Result<()> {
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
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut pipe = loop {
            match OpenOptions::new().write(true).open(endpoint) {
                Ok(pipe) => break pipe,
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
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

    pub(super) fn read_path(reader: &mut impl Read) -> io::Result<PathBuf> {
        read_path_units(reader).map(|units| PathBuf::from(OsString::from_wide(&units)))
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::{Claim, claim};

#[cfg(test)]
mod tests {
    use super::wire::{MAX_UNITS, read_path_units};
    use std::io::{self, Cursor};

    fn encode_units(units: &[u16]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + units.len() * 2);
        buf.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for &u in units {
            buf.extend_from_slice(&u.to_le_bytes());
        }
        buf
    }

    #[test]
    fn read_path_boundaries_roundtrip_and_reject_oversize_nul_or_truncated() {
        // 1. Empty path (bring window forward)
        assert!(
            read_path_units(&mut Cursor::new(encode_units(&[])))
                .unwrap()
                .is_empty()
        );

        // 2. Normal UTF-16LE CJK path
        let sample = r"D:\媒体库\测试 视频.mkv";
        let sample_u16: Vec<u16> = sample.encode_utf16().collect();
        let decoded = read_path_units(&mut Cursor::new(encode_units(&sample_u16))).unwrap();
        assert_eq!(String::from_utf16(&decoded).unwrap(), sample);

        // 3. Oversized length (> MAX_UNITS) rejected immediately before allocation
        let oversize = ((MAX_UNITS + 1) as u32).to_le_bytes();
        let err_big = read_path_units(&mut Cursor::new(oversize)).unwrap_err();
        assert_eq!(err_big.kind(), io::ErrorKind::InvalidData);

        // 4. Embedded NUL code unit rejected
        let err_nul = read_path_units(&mut Cursor::new(encode_units(&[65, 0, 66]))).unwrap_err();
        assert_eq!(err_nul.kind(), io::ErrorKind::InvalidData);

        // 5. Truncated header or truncated payload rejected
        assert_eq!(
            read_path_units(&mut Cursor::new([1u8, 0]))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut truncated = encode_units(&sample_u16);
        truncated.pop();
        assert_eq!(
            read_path_units(&mut Cursor::new(truncated))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
