//! Bounded native task supervisor. This controls lifetime and inherited state;
//! it is not an OS sandbox for an untrusted executable.
#[cfg(windows)]
pub(crate) mod windows;
use crate::vfs::HttpCancel;
use iris_api::v2::ErrorCode;
use std::{
    io::{Read, Write},
    process::{Command, ExitStatus},
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(windows)]
type NativeChild = windows::Child;
#[cfg(unix)]
type NativeChild = std::process::Child;
struct Child(NativeChild);
impl Child {
    fn status(&mut self) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(windows)]
        {
            self.0.status()
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            // WNOWAIT keeps the PID reserved until the process group is killed.
            let mut info: libc::siginfo_t;
            loop {
                info = unsafe { std::mem::zeroed() };
                if unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.0.id(),
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                } == 0
                {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            let status = unsafe { info.si_status() };
            let raw = if info.si_code == libc::CLD_EXITED {
                (status & 0xff) << 8
            } else if matches!(info.si_code, libc::CLD_KILLED | libc::CLD_DUMPED) {
                let sig = status & 0x7f;
                if sig == 0 { libc::SIGKILL } else { sig }
            } else {
                return Ok(None);
            };
            Ok(Some(ExitStatus::from_raw(raw)))
        }
    }
    fn terminate(&mut self) {
        #[cfg(windows)]
        self.0.close_job();
        #[cfg(unix)]
        {
            // Supported tools must not detach from the host process group.
            unsafe {
                libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.0.kill();
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.terminate();
        #[cfg(unix)]
        {
            let _ = self.0.wait();
        }
    }
}

/// Runs on a background worker. stdout may be a binary artifact sink with its
/// own disk quota; metadata callers use capture(). No pipe output is logged.
pub(crate) fn run(
    cmd: &mut Command,
    input: Option<Box<dyn Read + Send>>,
    cancel: &HttpCancel,
    timeout: Duration,
    stdout: &mut (dyn FnMut(&[u8]) -> Result<(), ErrorCode> + Send),
) -> Result<(), ErrorCode> {
    if !std::path::Path::new(cmd.get_program()).is_absolute() || cmd.get_current_dir().is_none() {
        return Err(ErrorCode::InvalidArgument);
    }
    if cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    #[cfg(windows)]
    let (child, stdin, out, err) = windows::spawn_clean(cmd).map_err(|_| ErrorCode::IoError)?;
    #[cfg(unix)]
    let (child, stdin, out, err) = {
        use std::{os::unix::process::CommandExt, process::Stdio};
        // No shell; Rust pipes are CLOEXEC. Also close inheritance of ambient
        // native descriptors created elsewhere in the application.
        unsafe {
            cmd.pre_exec(|| {
                cloexec_above_stdio();
                Ok(())
            });
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|_| ErrorCode::IoError)?;
        let stdin = child.stdin.take().ok_or(ErrorCode::Internal)?;
        let out = child.stdout.take().ok_or(ErrorCode::Internal)?;
        let err = child.stderr.take().ok_or(ErrorCode::Internal)?;
        (child, stdin, out, err)
    };
    let mut child = Child(child);
    let local = cancel.child().map_err(|_| ErrorCode::Cancelled)?;
    let error = Mutex::new(None);
    let fail = |value| {
        error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(value);
        local.cancel();
    };
    std::thread::scope(|scope| {
        let out_task = std::thread::Builder::new()
            .name("iris-tool-out".into())
            .spawn_scoped(scope, || drain(out, &local, stdout, &fail));
        let err_task = std::thread::Builder::new()
            .name("iris-tool-err".into())
            .spawn_scoped(scope, || {
                let mut count = 0usize;
                drain(
                    err,
                    &local,
                    &mut |bytes| {
                        count = count
                            .checked_add(bytes.len())
                            .ok_or(ErrorCode::QuotaExceeded)?;
                        if count > 1 << 20 {
                            return Err(ErrorCode::QuotaExceeded);
                        }
                        Ok(())
                    },
                    &fail,
                );
            });
        let in_task = std::thread::Builder::new()
            .name("iris-tool-in".into())
            .spawn_scoped(scope, || {
                let mut stdin = stdin;
                if let Some(mut input) = input {
                    let mut buffer = [0; 16384];
                    while !local.is_cancelled() {
                        match input.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                if stdin.write_all(&buffer[..n]).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                fail(ErrorCode::IoError);
                                break;
                            }
                        }
                    }
                }
            });
        if out_task.is_err() || err_task.is_err() || in_task.is_err() {
            fail(ErrorCode::Internal);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if local.is_cancelled() {
                break;
            }
            if Instant::now() >= deadline {
                fail(ErrorCode::Timeout);
                break;
            }
            match child.status() {
                Ok(Some(status)) => {
                    if !status.success() {
                        fail(ErrorCode::IoError);
                    }
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    fail(ErrorCode::IoError);
                    break;
                }
            }
        }
        child.terminate();
        // The leader and every inherited pipe are reaped before reservations
        // return to the caller. The UI never runs this function.
        for task in [out_task, err_task, in_task].into_iter().flatten() {
            if task.join().is_err() {
                fail(ErrorCode::Internal);
            }
        }
    });
    let failed = *error.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(error) = failed {
        return Err(error);
    }
    if cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    Ok(())
}
fn drain(
    mut reader: impl Read,
    cancel: &HttpCancel,
    sink: &mut (dyn FnMut(&[u8]) -> Result<(), ErrorCode> + Send),
    fail: &impl Fn(ErrorCode),
) {
    let mut bytes = [0; 16384];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) if !cancel.is_cancelled() => {
                if let Err(error) = sink(&bytes[..n]) {
                    fail(error);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                if !cancel.is_cancelled() {
                    fail(ErrorCode::IoError);
                }
                break;
            }
        }
    }
}
pub(crate) fn capture(
    cmd: &mut Command,
    cancel: &HttpCancel,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>, ErrorCode> {
    let mut bytes = Vec::new();
    run(cmd, None, cancel, timeout, &mut |chunk| {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ErrorCode::QuotaExceeded);
        }
        bytes.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(bytes)
}
#[cfg(unix)]
unsafe fn cloexec_above_stdio() {
    #[cfg(target_os = "linux")]
    if unsafe { libc::syscall(libc::SYS_close_range, 3usize, usize::MAX, 4usize) } == 0 {
        return;
    }
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        lim.rlim_cur.clamp(256, 65536) as libc::c_int
    } else {
        4096
    };
    for fd in 3..max {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(test)]
mod tests;
