//! Out-of-process stdio plugin transport, bounded JSON line reader, and wake signaling.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iris_api::{HostCommand, HostEvent, HostMessageRef, PluginManifest, PluginMessage};

/// Maximum JSON line accepted from an external plugin (2 MiB, fits large `ProvideSourcePage` payloads).
pub(super) const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDIN_QUEUE: usize = 64;
const MAX_STDOUT_QUEUE: usize = 256;
pub(super) const MAX_DRAIN_PER_POLL: usize = 256;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(150);
const MAX_STDERR_LINES: usize = 100;
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;

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
            Ok([]) => break,
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
    child: Child,
    stdin_tx: Option<SyncSender<Arc<String>>>,
    writer_thread: Option<JoinHandle<()>>,
    rx: Receiver<PluginMessage>,
    disconnected: bool,
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
        cmd.args(&manifest.args)
            .current_dir(&canonical_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("missing stdin pipe")?;
        let stdout = child.stdout.take().ok_or("missing stdout pipe")?;
        let stderr = child.stderr.take();

        let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Arc<String>>(MAX_STDIN_QUEUE);
        let pid_w = manifest.id.clone();
        let writer_thread = std::thread::Builder::new()
            .name(format!("iris-plugin-w-{pid_w}"))
            .spawn(move || {
                while let Ok(line) = stdin_rx.recv() {
                    if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
            })
            .ok();

        let (tx, rx) = mpsc::sync_channel(MAX_STDOUT_QUEUE);
        let plugin_id = manifest.id.clone();
        let wake_out = wake.clone();
        let _ = std::thread::Builder::new()
            .name(format!("iris-plugin-r-{plugin_id}"))
            .spawn(move || {
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
                            Ok(msg) => match tx.try_send(msg) {
                                Ok(()) => {
                                    if let Some(w) = &wake_out {
                                        w.notify();
                                    }
                                }
                                Err(TrySendError::Full(_)) => {
                                    log::warn!("plugin {plugin_id} output queue full; dropping");
                                }
                                Err(TrySendError::Disconnected(_)) => return false,
                            },
                            Err(e) => log::warn!("plugin {plugin_id} invalid JSON: {e}"),
                        }
                        true
                    },
                    || log::warn!("plugin {plugin_id} exceeded max line ({MAX_LINE_BYTES} B)"),
                );
                drop(tx);
                if let Some(w) = &wake_out {
                    w.notify();
                }
            });

        // Drain stderr continuously until EOF even after `MAX_STDERR_LINES` so plugins never get SIGPIPE.
        if let Some(stderr) = stderr {
            let pid_e = manifest.id.clone();
            let _ = std::thread::Builder::new()
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
                                    log::warn!(target: "iris::plugin::stderr", "[{pid_e}] {trimmed}");
                                    logged += 1;
                                }
                            }
                            true
                        },
                        || {},
                    );
                });
        }

        Ok(Self {
            child,
            stdin_tx: Some(stdin_tx),
            writer_thread,
            rx,
            disconnected: false,
        })
    }

    pub(super) fn send_encoded(
        &mut self,
        line: Arc<String>,
        droppable: bool,
    ) -> Result<(), String> {
        let tx = self.stdin_tx.as_ref().ok_or("plugin stdin closed")?;
        match tx.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) if droppable => Ok(()),
            Err(TrySendError::Full(_)) => Err("plugin stdin queue full (unresponsive)".into()),
            Err(TrySendError::Disconnected(_)) => Err("plugin stdin closed".into()),
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
        for _ in 0..MAX_DRAIN_PER_POLL {
            match self.rx.try_recv() {
                Ok(PluginMessage::Commands { commands }) => cmds.extend(commands),
                Ok(PluginMessage::Log { message }) => {
                    log::info!(target: "iris::plugin", "{message}")
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        let exited = self.disconnected.then(|| match self.child.try_wait() {
            Ok(Some(status)) => format!("进程已退出 ({status})"),
            Ok(None) => "输出通道已关闭".into(),
            Err(e) => format!("进程状态异常: {e}"),
        });
        (cmds, exited)
    }

    pub(super) fn stop(&mut self) {
        if let Some(tx) = self.stdin_tx.take()
            && let Ok(line) = encode_event(&HostEvent::Shutdown)
        {
            let _ = tx.try_send(line);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(w) = self.writer_thread.take() {
            let _ = w.join();
        }
    }
}

impl Drop for ExternalProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
