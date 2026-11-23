//! Out-of-process stdio plugin transport, bounded JSON line reader, and wake signaling.

use std::cell::Cell;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::process::{Command as ProcessCommand, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iris_api::{HostCommand, HostEvent, HostMessageRef, PluginManifest, PluginMessage};

#[cfg(windows)]
#[path = "windows_process.rs"]
mod windows;
#[cfg(windows)]
use windows::Child;

/// Maximum JSON line accepted from an external plugin (2 MiB, fits large `ProvideSourcePage` payloads).
pub(super) const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDIN_QUEUE: usize = 64;
const MAX_STDOUT_QUEUE: usize = 8;
pub(super) const MAX_DRAIN_PER_POLL: usize = 256;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(150);
const STATUS_INTERVAL: Duration = Duration::from_millis(15);
const MAX_STDERR_LINES: usize = 100;
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;

// The two callbacks of the line reader share one allowance per process/stream.
struct LogBudget(Cell<u16>);

impl LogBudget {
    fn take(&self) -> bool {
        let Some(left) = self.0.get().checked_sub(1) else {
            return false;
        };
        self.0.set(left);
        true
    }
}

/// Coalesced event-driven wake signal from plugin background threads to the UI loop.
#[derive(Clone)]
pub(super) struct WakeSignal {
    tx: UnboundedSender<()>,
    pending: Arc<AtomicBool>,
}

impl WakeSignal {
    pub(super) fn new() -> (Self, UnboundedReceiver<()>) {
        let (tx, rx) = unbounded();
        (
            Self {
                tx,
                pending: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    pub(super) fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.unbounded_send(());
        }
    }

    pub(super) fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

pub(super) fn encode_event(event: &HostEvent) -> Result<Arc<String>, String> {
    let mut line = serde_json::to_string(&HostMessageRef::Event { payload: event })
        .map_err(|e| e.to_string())?;
    line.push('\n');
    Ok(Arc::new(line))
}

/// Reads newline-delimited lines with a strict byte bound even without `\n`.
pub(super) fn read_bounded_lines(
    reader: impl Read,
    max_bytes: usize,
    mut on_line: impl FnMut(&[u8]) -> bool,
    mut on_oversized: impl FnMut(),
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::with_capacity(1024);
    let mut discarding = false;
    loop {
        let available = match reader.fill_buf() {
            Ok([]) => {
                if !discarding && !buf.is_empty() {
                    on_line(&buf);
                }
                break;
            }
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let nl = available.iter().position(|&b| b == b'\n');
        let count = nl.map_or(available.len(), |i| i + 1);
        if discarding {
            reader.consume(count);
            discarding = nl.is_none();
            continue;
        }
        if buf.len() + count > max_bytes {
            buf.clear();
            on_oversized();
            reader.consume(count);
            discarding = nl.is_none();
            continue;
        }
        buf.extend_from_slice(&available[..count]);
        reader.consume(count);
        if nl.is_some() {
            let keep_going = on_line(&buf);
            buf.clear();
            if !keep_going {
                break;
            }
        }
    }
}

pub(super) struct ExternalProcess {
    stop_tx: Option<mpsc::Sender<()>>,
    stopped: Receiver<()>,
    exit_rx: Receiver<(Instant, String)>,
    exit: Option<(Instant, String)>,
    wake: Option<WakeSignal>,
    stdin_tx: Option<SyncSender<(Arc<String>, bool)>>,
    tick_pending: Arc<AtomicBool>,
    rx: Receiver<Vec<HostCommand>>,
    disconnected: bool,
}

#[cfg(unix)]
fn child_status(child: &Child) -> std::io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    // Keep the leader waitable until the group has been signalled. Reaping it early
    // could let the OS recycle its process-group ID before shutdown.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let code = unsafe { info.si_status() };
    let raw = match info.si_code {
        libc::CLD_EXITED => code << 8,
        libc::CLD_DUMPED => code | 0x80,
        _ => code,
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

#[cfg(windows)]
fn child_status(child: &Child) -> std::io::Result<Option<ExitStatus>> {
    child.status()
}

fn exit_reason(status: ExitStatus) -> String {
    #[cfg(target_os = "macos")]
    if std::os::unix::process::ExitStatusExt::signal(&status) == Some(libc::SIGKILL) {
        return format!(
            "进程已退出 ({status})。请查看系统日志并核验插件来源、签名和 Gatekeeper 提示。"
        );
    }
    format!("进程已退出 ({status})")
}

// Unix process groups are cooperative lifecycle management. If a helper leaves
// the group while retaining a pipe, the host still needs to release its own I/O.
#[cfg(unix)]
struct CancellablePipe<T> {
    pipe: T,
    cancelled: Arc<AtomicBool>,
}

#[cfg(unix)]
impl<T: AsRawFd> CancellablePipe<T> {
    fn new(pipe: T, cancelled: &Arc<AtomicBool>) -> std::io::Result<Self> {
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            pipe,
            cancelled: Arc::clone(cancelled),
        })
    }

    fn io(
        &mut self,
        events: libc::c_short,
        mut f: impl FnMut(&mut T) -> std::io::Result<usize>,
    ) -> std::io::Result<usize> {
        let mut poll = libc::pollfd {
            fd: self.pipe.as_raw_fd(),
            events,
            revents: 0,
        };
        while !self.cancelled.load(Ordering::Acquire) {
            match f(&mut self.pipe) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if unsafe { libc::poll(&mut poll, 1, STATUS_INTERVAL.as_millis() as i32) } < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() != ErrorKind::Interrupted {
                            return Err(e);
                        }
                    }
                }
                result => return result,
            }
        }
        Err(ErrorKind::BrokenPipe.into())
    }
}

#[cfg(unix)]
impl<T: Read + AsRawFd> Read for CancellablePipe<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.io(libc::POLLIN, |pipe| pipe.read(buf))
    }
}

#[cfg(unix)]
impl<T: Write + AsRawFd> Write for CancellablePipe<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.io(libc::POLLOUT, |pipe| pipe.write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pipe.flush()
    }
}

fn terminate_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // Signal while the leader still owns its PID, before reaping it.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.kill();
    }
    #[cfg(windows)]
    child.close_job();
}

fn supervise(
    mut child: Child,
    stop: Receiver<()>,
    stopped: mpsc::Sender<()>,
    exit: mpsc::Sender<(Instant, String)>,
    pipe_closed: &AtomicBool,
    #[cfg(unix)] cancelled: &AtomicBool,
    wake: Option<WakeSignal>,
) {
    let mut shutdown_at = None;
    let mut closed_at = None;
    let reason = loop {
        match child_status(&child) {
            Ok(Some(status)) => break Some(exit_reason(status)),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => break Some(format!("进程状态异常: {e}")),
            Ok(None) => {}
        }
        let now = Instant::now();
        if shutdown_at.is_some_and(|when| now >= when) {
            break None;
        }
        if pipe_closed.load(Ordering::Acquire)
            && now >= *closed_at.get_or_insert(now + SHUTDOWN_GRACE)
        {
            break Some("插件通信通道已关闭".into());
        }
        if shutdown_at.is_some() {
            std::thread::sleep(STATUS_INTERVAL);
        } else if stop
            .recv_timeout(STATUS_INTERVAL)
            .is_err_and(|e| e == mpsc::RecvTimeoutError::Disconnected)
        {
            shutdown_at = Some(Instant::now() + SHUTDOWN_GRACE);
        }
    };
    terminate_group(&mut child);
    let _ = stopped.send(());
    if let Some(reason) = reason {
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        let _ = exit.send((deadline, reason));
        if let Some(w) = &wake {
            w.notify();
        }
        // An escaped helper may hold stdout open. Give the reader time to deliver
        // queued output, then wake the UI so it can retire the dead leader anyway.
        if let Some(w) = wake {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            w.notify();
        }
    }
    #[cfg(unix)]
    cancelled.store(true, Ordering::Release);
    #[cfg(unix)]
    let _ = child.wait();
}

impl ExternalProcess {
    pub(super) fn spawn(
        dir: &Path,
        manifest: &PluginManifest,
        wake: Option<WakeSignal>,
    ) -> Result<Self, String> {
        let entry = manifest.entry.as_deref().ok_or("manifest missing entry")?;
        let canonical_dir = dir
            .canonicalize()
            .map_err(|e| format!("invalid plugin dir: {e}"))?;
        let entry_path = dir.join(entry);
        if !entry_path.exists() {
            return Err(format!("entry not found: {}", entry_path.display()));
        }
        let canonical_entry = entry_path
            .canonicalize()
            .map_err(|e| format!("cannot resolve entry: {e}"))?;
        if !canonical_entry.starts_with(&canonical_dir) {
            return Err("entry resolves outside plugin directory".into());
        }

        let is_py = canonical_entry
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("py"));
        let mut cmd = if is_py {
            let py = if cfg!(target_os = "windows") {
                "python"
            } else {
                "python3"
            };
            let mut c = ProcessCommand::new(py);
            c.env("PYTHONUTF8", "1")
                .env("PYTHONIOENCODING", "utf-8")
                .arg("-u")
                .arg(&canonical_entry);
            c
        } else {
            ProcessCommand::new(&canonical_entry)
        };
        cmd.args(&manifest.args).current_dir(&canonical_dir);
        cmd.env_remove("IRIS_CONTROL");
        #[cfg(windows)]
        let (child, mut stdin, stdout, stderr) =
            windows::spawn(&cmd).map_err(|e| format!("plugin job launch failed: {e}"))?;
        #[cfg(unix)]
        let (child, stdin, stdout, stderr) = {
            use std::os::unix::process::CommandExt;
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .map_err(|e| {
                    #[cfg(target_os = "macos")]
                    if e.kind() == ErrorKind::PermissionDenied {
                        return format!(
                            "spawn failed: {e}. 请核验插件来源、执行权限、签名和 Gatekeeper 提示。"
                        );
                    }
                    format!("spawn failed: {e}")
                })?;
            let stdin = child.stdin.take().ok_or("missing stdin pipe")?;
            let stdout = child.stdout.take().ok_or("missing stdout pipe")?;
            let stderr = child.stderr.take().ok_or("missing stderr pipe")?;
            (child, stdin, stdout, stderr)
        };

        let pipe_closed = Arc::new(AtomicBool::new(false));
        let (child_tx, child_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (stopped_tx, stopped) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let supervisor_wake = wake.clone();
        let supervisor_pipe = Arc::clone(&pipe_closed);
        #[cfg(unix)]
        let cancelled = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        let supervisor_cancel = Arc::clone(&cancelled);
        if let Err(e) = std::thread::Builder::new()
            .name("iris-plugin-supervisor".into())
            .spawn(move || {
                if let Ok(child) = child_rx.recv() {
                    supervise(
                        child,
                        stop_rx,
                        stopped_tx,
                        exit_tx,
                        &supervisor_pipe,
                        #[cfg(unix)]
                        &supervisor_cancel,
                        supervisor_wake,
                    );
                }
            })
        {
            let mut child = child;
            terminate_group(&mut child);
            #[cfg(unix)]
            let _ = child.try_wait();
            return Err(format!("plugin supervisor thread failed: {e}"));
        }
        let _ = child_tx.send(child);
        #[cfg(unix)]
        let (mut stdin, stdout, stderr) = {
            let error = |e| format!("cannot make plugin pipes cancellable: {e}");
            (
                CancellablePipe::new(stdin, &cancelled).map_err(error)?,
                CancellablePipe::new(stdout, &cancelled).map_err(error)?,
                CancellablePipe::new(stderr, &cancelled).map_err(error)?,
            )
        };

        let (stdin_tx, stdin_rx) = mpsc::sync_channel::<(Arc<String>, bool)>(MAX_STDIN_QUEUE);
        let tick_pending = Arc::new(AtomicBool::new(false));
        let writer_tick = Arc::clone(&tick_pending);
        let writer_closed = Arc::clone(&pipe_closed);
        let pid_w = manifest.id.clone();
        let writer = std::thread::Builder::new()
            .name(format!("iris-plugin-w-{pid_w}"))
            .spawn(move || {
                while let Ok((line, tick)) = stdin_rx.recv() {
                    if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
                        writer_closed.store(true, Ordering::Release);
                        return;
                    }
                    if tick {
                        writer_tick.store(false, Ordering::Relaxed);
                    }
                }
                // Disconnecting the sender requests shutdown without competing for a queue slot.
                // Dropping this thread's stdin handle then delivers EOF to the protocol reader.
                if let Ok(shutdown) = encode_event(&HostEvent::Shutdown) {
                    let _ = stdin
                        .write_all(shutdown.as_bytes())
                        .and_then(|()| stdin.flush());
                }
            });

        let (tx, rx) = mpsc::sync_channel(MAX_STDOUT_QUEUE);
        let plugin_id = manifest.id.clone();
        let wake_out = wake.clone();
        let reader = std::thread::Builder::new()
            .name(format!("iris-plugin-r-{plugin_id}"))
            .spawn(move || {
                let warnings = LogBudget(Cell::new(16));
                let logs = LogBudget(Cell::new(100));
                read_bounded_lines(
                    stdout,
                    MAX_LINE_BYTES,
                    |buf| {
                        let Ok(line) = std::str::from_utf8(buf) else {
                            return true;
                        };
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            return true;
                        }
                        match serde_json::from_str::<PluginMessage>(trimmed) {
                            Ok(PluginMessage::Commands { mut commands }) => {
                                // Protect the inter-thread queue too. One oversized overlay does
                                // not invalidate a source-page reply in the same batch.
                                commands.retain(|command| {
                                    if let HostCommand::SetOverlay { nodes } = command
                                        && !super::slots::overlay_fits(nodes)
                                    {
                                        if warnings.take() {
                                            log::warn!("plugin {plugin_id} exceeded overlay budget");
                                        }
                                        return false;
                                    }
                                    true
                                });
                                if !commands.is_empty() {
                                    if tx.send(commands).is_err() {
                                        return false;
                                    }
                                    if let Some(w) = &wake_out {
                                        w.notify();
                                    }
                                }
                            }
                            Ok(PluginMessage::Log { message }) if logs.take() => {
                                let end = message.floor_char_boundary(MAX_STDERR_LINE_BYTES.min(message.len()));
                                log::info!(target: "iris::plugin", "[{plugin_id}] {}", message[..end].escape_debug());
                            }
                            Err(e) if warnings.take() => log::warn!("plugin {plugin_id} invalid JSON (line {}, column {})", e.line(), e.column()),
                            _ => {}
                        }
                        true
                    },
                    || { if warnings.take() { log::warn!("plugin {plugin_id} exceeded max line ({MAX_LINE_BYTES} B)"); } },
                );
                drop(tx);
                pipe_closed.store(true, Ordering::Release);
                if let Some(w) = &wake_out {
                    w.notify();
                }
            });

        // Drain stderr continuously until EOF even after `MAX_STDERR_LINES` so plugins never get SIGPIPE.
        let stderr_reader = {
            let pid_e = manifest.id.clone();
            std::thread::Builder::new()
                .name(format!("iris-plugin-e-{pid_e}"))
                .spawn(move || {
                    let mut logged = 0usize;
                    read_bounded_lines(
                        stderr,
                        MAX_STDERR_LINE_BYTES,
                        |buf| {
                            if logged < MAX_STDERR_LINES {
                                let text = String::from_utf8_lossy(buf);
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    log::warn!(target: "iris::plugin::stderr", "[{pid_e}] {}", trimmed.escape_debug());
                                    logged += 1;
                                }
                            }
                            true
                        },
                        || {},
                    );
                })
        };

        let process = Self {
            stop_tx: Some(stop_tx),
            stopped,
            exit_rx,
            exit: None,
            wake,
            stdin_tx: Some(stdin_tx),
            tick_pending,
            rx,
            disconnected: false,
        };
        writer
            .and(reader)
            .and(stderr_reader)
            .map(|_| ())
            .map_err(|e| format!("plugin pipe thread failed: {e}"))?;
        Ok(process)
    }

    pub(super) fn send_encoded(
        &mut self,
        line: Arc<String>,
        droppable: bool,
    ) -> Result<(), String> {
        let tx = self.stdin_tx.as_ref().ok_or("plugin stdin closed")?;
        // One pending tick is enough. A burst of position updates must leave room for user commands.
        if droppable && self.tick_pending.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        match tx.try_send((line, droppable)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) if droppable => {
                self.tick_pending.store(false, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err("plugin stdin queue full (unresponsive)".into()),
            // The supervisor reports the actual exit status if the writer closes first.
            Err(TrySendError::Disconnected(_)) => Ok(()),
        }
    }

    pub(super) fn send(&mut self, event: &HostEvent) -> Result<(), String> {
        self.send_encoded(
            encode_event(event)?,
            matches!(event, HostEvent::Tick { .. }),
        )
    }

    pub(super) fn drain(&mut self) -> (Vec<HostCommand>, Option<String>) {
        let mut cmds = Vec::new();
        let mut drained = 0;
        for _ in 0..MAX_STDOUT_QUEUE {
            match self.rx.try_recv() {
                Ok(commands) => {
                    cmds.extend(commands);
                    drained += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        if self.exit.is_none() {
            self.exit = self.exit_rx.try_recv().ok();
        }
        let exited = if drained == MAX_STDOUT_QUEUE {
            if let Some(w) = &self.wake {
                w.notify();
            }
            None
        } else if self.disconnected
            || self
                .exit
                .as_ref()
                .is_some_and(|(deadline, _)| Instant::now() >= *deadline)
        {
            self.exit.take().map(|(_, reason)| reason)
        } else {
            None
        };
        (cmds, exited)
    }

    pub(super) fn request_shutdown(&mut self) {
        drop(self.stdin_tx.take());
        drop(self.stop_tx.take());
    }

    pub(super) fn wait_for_termination(&self, deadline: Instant) {
        let _ = self
            .stopped
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

impl Drop for ExternalProcess {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_line_reader_preserves_the_final_unterminated_line() {
        let mut lines = Vec::new();
        read_bounded_lines(
            &b"first\nlast"[..],
            8,
            |line| {
                lines.push(line.to_vec());
                true
            },
            || panic!("valid lines must fit"),
        );
        assert_eq!(lines, [b"first\n".to_vec(), b"last".to_vec()]);
    }

    #[test]
    fn log_allowance_stays_exhausted() {
        let budget = LogBudget(Cell::new(2));
        assert!(budget.take());
        assert!(budget.take());
        for _ in 0..1024 {
            assert!(!budget.take());
        }
    }

    #[test]
    fn line_limit_accepts_full_two_mib_frame_and_resynchronizes() {
        let mut bytes = vec![b'a'; MAX_LINE_BYTES - 1];
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(b'b', MAX_LINE_BYTES));
        bytes.extend_from_slice(b"\nok\n");
        let mut sizes = Vec::new();
        let mut dropped = 0;
        read_bounded_lines(
            bytes.as_slice(),
            MAX_LINE_BYTES,
            |line| {
                sizes.push(line.len());
                true
            },
            || dropped += 1,
        );
        assert_eq!(sizes, [MAX_LINE_BYTES, 3]);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn bounded_line_reader_skips_oversized_lines_without_allocating() {
        let mut input = vec![b'x'; 200];
        input.extend_from_slice(b"\ngood\n");
        let (mut lines, mut oversized) = (Vec::new(), 0usize);
        read_bounded_lines(
            std::io::Cursor::new(input),
            64,
            |b| {
                lines.push(String::from_utf8_lossy(b).trim().to_string());
                true
            },
            || oversized += 1,
        );
        assert_eq!(
            (oversized, lines.as_slice()),
            (1, &["good".to_string()][..])
        );
    }
}
