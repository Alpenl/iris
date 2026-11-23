//! Windows file associations forward each open selection to the running instance
//! of this user and logon session. The versioned wire format carries only bounded
//! UTF-16 paths, with an empty batch requesting window activation.
//! macOS needs none of this: LaunchServices delivers opens to the running app.

#[cfg(any(target_os = "windows", test))]
mod wire {
    use std::io::{self, Read};

    /// Longest Windows path in UTF-16 units (the `\\?\` limit).
    pub(super) const MAX_UNITS: usize = 32_767;
    pub(super) const MAX_PATHS: usize = 16_384;
    pub(super) const MAX_BATCH_UNITS: usize = 1_048_576;

    fn read_length(reader: &mut impl Read) -> io::Result<usize> {
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes) as usize)
    }

    pub(super) fn read_batch<T>(
        reader: &mut impl Read,
        decode: impl Fn(&[u16]) -> T,
    ) -> io::Result<Vec<T>> {
        let count = read_length(reader)?;
        if count > MAX_PATHS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many file paths",
            ));
        }
        let mut remaining = MAX_BATCH_UNITS;
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            let units = read_path_units(reader, remaining)?;
            remaining -= units.len();
            paths.push(decode(&units));
        }
        Ok(paths)
    }

    /// Reads a length-prefixed UTF-16LE path payload with strict size and NUL checks.
    fn read_path_units(reader: &mut impl Read, remaining: usize) -> io::Result<Vec<u16>> {
        let len = read_length(reader)?;
        if len == 0 || len > MAX_UNITS.min(remaining) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid file path length",
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
    use std::io::{self, Write};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GetLastError,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, DisconnectNamedPipe, GetNamedPipeServerSessionId,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};

    use super::wire::{MAX_BATCH_UNITS, MAX_PATHS, MAX_UNITS, read_batch};

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
    pub(crate) fn claim(initial: &[PathBuf], opened: UnboundedSender<Vec<PathBuf>>) -> Claim {
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
        let endpoint = format!(r"\\.\pipe\iris-open-batch-{session}-{sid}");
        let name: Vec<u16> = format!(r"Local\Iris.Player.Batch.{sid}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
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

    fn forward(endpoint: &str, session: u32, paths: &[PathBuf]) -> io::Result<()> {
        if paths.len() > MAX_PATHS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many file paths",
            ));
        }
        let mut message = (paths.len() as u32).to_le_bytes().to_vec();
        let mut remaining = MAX_BATCH_UNITS;
        for path in paths {
            let path = std::path::absolute(path)?;
            let start = message.len();
            message.extend_from_slice(&[0; 4]);
            let mut len = 0;
            for unit in path.as_os_str().encode_wide() {
                if len == MAX_UNITS.min(remaining) || unit == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid file path batch",
                    ));
                }
                message.extend_from_slice(&unit.to_le_bytes());
                len += 1;
            }
            message[start..start + 4].copy_from_slice(&(len as u32).to_le_bytes());
            remaining -= len;
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
        pipe.write_all(&message)
    }

    fn serve(mut pipe: std::fs::File, opened: UnboundedSender<Vec<PathBuf>>) {
        while !opened.is_closed() {
            // SAFETY: the pipe handle is owned by `pipe` for the whole loop.
            if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    log::error!("accepting file-open pipe: {error}");
                    return;
                }
            }
            match read_batch(&mut pipe, |units| PathBuf::from(OsString::from_wide(units))) {
                Ok(paths) => {
                    let _ = opened.unbounded_send(paths);
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
}

#[cfg(target_os = "windows")]
pub(crate) use imp::{Claim, claim};

#[cfg(test)]
mod tests {
    use super::wire::{MAX_PATHS, MAX_UNITS, read_batch};
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
    fn wire_batches_preserve_order_unicode_and_activation() {
        let decode = |units: &[u16]| units.to_vec();
        assert!(
            read_batch(&mut Cursor::new([0; 4]), decode)
                .unwrap()
                .is_empty()
        );
        let names: Vec<Vec<_>> = [r"D:\媒体库\测试 视频.mkv", r"D:\媒体库\中文字幕.srt"]
            .iter()
            .map(|s| s.encode_utf16().collect())
            .collect();
        let mut data = 2u32.to_le_bytes().to_vec();
        for name in &names {
            data.extend(encode_units(name));
        }
        assert_eq!(read_batch(&mut Cursor::new(data), decode).unwrap(), names);
    }

    #[test]
    fn wire_rejects_whole_batch_on_invalid_path_or_count() {
        for invalid in [
            encode_units(&[]),
            encode_units(&[65, 0, 66]),
            ((MAX_UNITS + 1) as u32).to_le_bytes().to_vec(),
            vec![2, 0, 0, 0, 1],
        ] {
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend(encode_units(&[65]));
            data.extend(invalid);
            assert!(read_batch(&mut Cursor::new(data), |_| ()).is_err());
        }
        let count = ((MAX_PATHS + 1) as u32).to_le_bytes();
        assert_eq!(
            read_batch(&mut Cursor::new(count), |_| ())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
