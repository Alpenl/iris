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
    use futures::channel::{mpsc::Sender, oneshot};
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::PathBuf;
    use std::ptr::null;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, GetLastError,
    };
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, SECURITY_SQOS_PRESENT, WriteFile};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, DisconnectNamedPipe, GetNamedPipeClientSessionId,
        GetNamedPipeServerProcessId, GetNamedPipeServerSessionId, PIPE_NOWAIT, PIPE_WAIT,
        SetNamedPipeHandleState,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};

    use super::wire::{MAX_BATCH_UNITS, MAX_PATHS, MAX_UNITS, read_batch};

    pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(3);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    const ACCEPTED: u8 = 1;
    const REJECTED: u8 = 0;

    pub(crate) struct OpenRequest {
        pub(crate) paths: Vec<PathBuf>,
        pub(crate) ack: oneshot::Sender<()>,
    }

    /// Holds the per-session mutex for the life of the primary instance.
    pub(crate) struct Guard {
        _handle: Option<OwnedHandle>,
    }

    pub(crate) enum Claim {
        /// This process is the primary instance; keep the guard alive.
        Primary(Guard),
        /// The path went to the running instance; this process should exit.
        Forwarded,
        /// The primary instance could not accept this request.
        Rejected,
    }

    pub(super) fn session_id() -> io::Result<u32> {
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
    pub(crate) fn claim(initial: &[PathBuf], opened: Sender<OpenRequest>) -> Claim {
        if std::env::var_os("IRIS_SETTINGS").is_some() {
            return Claim::Primary(Guard { _handle: None });
        }
        let (session, sid) = match (session_id(), crate::platform::pipe::current_user_sid()) {
            (Ok(session), Ok(sid)) => (session, sid),
            (Err(e), _) | (_, Err(e)) => {
                log::error!("single instance: {e}");
                return Claim::Rejected;
            }
        };
        let endpoint = format!(r"\\.\pipe\iris-open-batch-v2-{session}-{sid}");
        let name: Vec<u16> = format!(r"Local\Iris.Player.Batch.{sid}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: NUL-terminated name; a non-null handle immediately gains an owner.
        let (mutex, exists) = unsafe {
            let handle = CreateMutexW(null(), 0, name.as_ptr());
            if handle.is_null() {
                log::error!("single-instance mutex: {}", io::Error::last_os_error());
                return Claim::Rejected;
            }
            let exists = GetLastError() == ERROR_ALREADY_EXISTS;
            (OwnedHandle::from_raw_handle(handle), exists)
        };
        if exists {
            return match forward(&endpoint, session, initial) {
                Ok(()) => Claim::Forwarded,
                Err(e) => {
                    log::error!("forwarding to the running instance: {e}");
                    Claim::Rejected
                }
            };
        }
        let wide: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
        let pipe = match crate::platform::pipe::create_duplex(&wide, (MAX_UNITS * 2 + 4) as u32) {
            Ok(pipe) => pipe,
            Err(e) => {
                log::error!("file-open pipe: {e}");
                return Claim::Rejected;
            }
        };
        if let Err(e) = iris_net::concurrency::spawn_thread(
            iris_net::concurrency::ThreadRole::SingleInstanceIpc,
            move || serve(pipe, session, opened),
        ) {
            log::error!("starting file-open listener: {e}");
            return Claim::Rejected;
        }
        Claim::Primary(Guard {
            _handle: Some(mutex),
        })
    }

    pub(super) fn forward(endpoint: &str, session: u32, paths: &[PathBuf]) -> io::Result<()> {
        let non_empty: Vec<&PathBuf> = paths.iter().filter(|p| !p.as_os_str().is_empty()).collect();
        if non_empty.len() > MAX_PATHS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many file paths",
            ));
        }
        let mut message = (non_empty.len() as u32).to_le_bytes().to_vec();
        let mut remaining = MAX_BATCH_UNITS;
        for path in non_empty {
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
        let deadline = Instant::now() + IO_TIMEOUT;
        let pipe = loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(SECURITY_SQOS_PRESENT)
                .open(endpoint)
            {
                Ok(pipe) => break pipe,
                Err(e) if matches!(e.raw_os_error(), Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32) =>
                {
                    wait_for_io(deadline)?;
                }
                Err(e) => return Err(e),
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
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Windows only honors the grant when this launch is allowed to set focus.
        let _ = unsafe { AllowSetForegroundWindow(pid) };
        set_pipe_mode(&pipe, PIPE_NOWAIT)?;
        let mut io = PipeIo {
            pipe: &pipe,
            deadline,
        };
        io.write_all(&message)?;
        let mut reply = [0];
        io.read_exact(&mut reply)?;
        // Confirm the read before the server disconnects and discards unread bytes.
        let _ = io.write_all(&reply);
        if reply[0] == ACCEPTED {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "primary window is closing or unavailable",
            ))
        }
    }

    fn serve(pipe: File, session: u32, mut opened: Sender<OpenRequest>) {
        while !opened.is_closed() {
            if let Err(error) = serve_connection(&pipe, session, &mut opened) {
                log::error!("file-open pipe listener: {error}");
                return;
            }
        }
    }

    pub(super) fn serve_connection(
        pipe: &File,
        session: u32,
        opened: &mut Sender<OpenRequest>,
    ) -> io::Result<()> {
        // The idle accept blocks. Connected reads and writes have a deadline.
        let connected =
            if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } != 0 {
                true
            } else {
                let error = io::Error::last_os_error();
                match error.raw_os_error().map(|code| code as u32) {
                    Some(ERROR_PIPE_CONNECTED) => true,
                    // A client can open and close before ConnectNamedPipe runs.
                    Some(ERROR_NO_DATA) => false,
                    _ => return Err(error),
                }
            };
        if connected && let Err(e) = handle_request(pipe, session, opened) {
            log::warn!("file-open message: {e}");
        }
        if unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        set_pipe_mode(pipe, PIPE_WAIT)
    }

    fn handle_request(
        pipe: &File,
        session: u32,
        opened: &mut Sender<OpenRequest>,
    ) -> io::Result<()> {
        let mut peer_session = u32::MAX;
        if unsafe { GetNamedPipeClientSessionId(pipe.as_raw_handle(), &mut peer_session) } == 0
            || peer_session != session
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pipe client is in another session",
            ));
        }
        set_pipe_mode(pipe, PIPE_NOWAIT)?;
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut io = PipeIo { pipe, deadline };
        let paths = read_batch(&mut io, |units| PathBuf::from(OsString::from_wide(units)))?;
        let (ack, mut acknowledged) = oneshot::channel();
        let accepted = opened.try_send(OpenRequest { paths, ack }).is_ok()
            && loop {
                match acknowledged.try_recv() {
                    Ok(Some(())) => break true,
                    Err(_) => break false,
                    Ok(None) if wait_for_io(deadline).is_err() => break false,
                    Ok(None) => {}
                }
            };
        drop(acknowledged);
        // Reserve a short response window even if the UI took the whole deadline.
        io.deadline = Instant::now() + Duration::from_millis(100);
        let response = if accepted { ACCEPTED } else { REJECTED };
        io.write_all(&[response])?;
        let mut receipt = [0];
        io.read_exact(&mut receipt)?;
        Ok(())
    }

    fn set_pipe_mode(pipe: &File, mode: u32) -> io::Result<()> {
        if unsafe { SetNamedPipeHandleState(pipe.as_raw_handle(), &mode, null(), null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn wait_for_io(deadline: Instant) -> io::Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "file-open pipe timed out",
            ));
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
        Ok(())
    }

    struct PipeIo<'a> {
        pipe: &'a File,
        deadline: Instant,
    }

    impl PipeIo<'_> {
        fn transfer(&self, mut call: impl FnMut(*mut u32) -> i32) -> io::Result<usize> {
            loop {
                if Instant::now() >= self.deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "file-open pipe timed out",
                    ));
                }
                let mut transferred = 0;
                if call(&mut transferred) != 0 {
                    if transferred != 0 {
                        return Ok(transferred as usize);
                    }
                } else {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(ERROR_NO_DATA as i32) {
                        return Err(e);
                    }
                }
                wait_for_io(self.deadline)?;
            }
        }
    }

    impl Read for PipeIo<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            self.transfer(|transferred| unsafe {
                ReadFile(
                    self.pipe.as_raw_handle(),
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                    transferred,
                    std::ptr::null_mut(),
                )
            })
        }
    }

    impl Write for PipeIo<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            self.transfer(|transferred| unsafe {
                WriteFile(
                    self.pipe.as_raw_handle(),
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    transferred,
                    std::ptr::null_mut(),
                )
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            // Writes go straight to the pipe. The receipt synchronizes final delivery.
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use imp::{Claim, OpenRequest, claim};

#[cfg(test)]
mod tests {
    use super::wire::{MAX_PATHS, MAX_UNITS, read_batch};
    use std::io::{self, Cursor};

    #[cfg(target_os = "windows")]
    mod win {
        use super::super::imp::*;
        use futures::channel::mpsc::{self, Receiver, TryRecvError};
        use std::io;
        use std::sync::mpsc as sync;
        use std::thread;
        use std::time::{Duration, Instant};

        fn request_from(receiver: &mut Receiver<OpenRequest>) -> OpenRequest {
            let deadline = Instant::now() + IO_TIMEOUT;
            loop {
                match receiver.try_recv() {
                    Ok(request) => return request,
                    Err(TryRecvError::Empty) => wait_for_io(deadline).unwrap(),
                    Err(error) => panic!("{error}"),
                }
            }
        }

        #[test]
        fn activation_waits_for_admission_and_unanswered_requests_expire() {
            let session = session_id().unwrap();
            let endpoint = format!(r"\\.\pipe\iris-test-open-{}", std::process::id());
            let wide: Vec<_> = endpoint.encode_utf16().chain(Some(0)).collect();
            let pipe = crate::platform::pipe::create_duplex(&wide, 4096).unwrap();
            let (mut opened, mut receiver) = mpsc::channel(0);
            let server = thread::spawn(move || {
                for _ in 0..2 {
                    serve_connection(&pipe, session, &mut opened).unwrap();
                }
            });

            let (finished, result) = sync::channel();
            let first = endpoint.clone();
            let client =
                thread::spawn(move || finished.send(forward(&first, session, &[])).unwrap());
            let request = request_from(&mut receiver);
            assert!(request.paths.is_empty());
            assert!(matches!(
                result.recv_timeout(Duration::from_millis(30)),
                Err(sync::RecvTimeoutError::Timeout)
            ));
            request.ack.send(()).unwrap();
            result.recv_timeout(IO_TIMEOUT).unwrap().unwrap();
            client.join().unwrap();

            let (finished, result) = sync::channel();
            let client =
                thread::spawn(move || finished.send(forward(&endpoint, session, &[])).unwrap());
            let request = request_from(&mut receiver);
            let error = result
                .recv_timeout(IO_TIMEOUT + Duration::from_secs(1))
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused
            ));
            client.join().unwrap();
            server.join().unwrap();
            assert!(request.ack.send(()).is_err());
        }
    }

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
