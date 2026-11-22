//! Control-plane & vector-overlay plugin manager.
//!
//! Manages both built-in official plugins and external out-of-process plugins
//! discovered under `<app_data_dir>/plugins/<id>/plugin.json` (or `IRIS_PLUGINS`).
//! External plugins communicate via newline-delimited JSON over stdio using
//! [`iris_api::HostMessage`] and [`iris_api::PluginMessage`].

mod builtins;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iris_api::{
    BuiltinPlugin, CommandSpec, HostCommand, HostEvent, HostMessageRef, MediaKind, OverlayShape,
    PROTOCOL_VERSION, Permission, PlaybackSnapshot, PluginManifest, PluginMessage, SidebarPanel,
    StatusItem,
};

use crate::library::Kind;
use crate::player::Session;
use crate::settings;

use builtins::{AbLoopPlugin, GuidesPlugin};

/// Maximum JSON line accepted from an external plugin (64 KiB).
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Maximum queued outgoing events per external plugin (prevents UI blocking).
const MAX_STDIN_QUEUE: usize = 64;
/// Maximum queued incoming messages per external plugin (prevents OOM).
const MAX_STDOUT_QUEUE: usize = 256;
/// Maximum queued plugin messages drained in a single host poll.
const MAX_DRAIN_PER_POLL: usize = 256;
/// Grace period for an external plugin to exit cleanly after `Shutdown`.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(150);
/// Maximum stderr lines logged per external plugin process.
const MAX_STDERR_LINES: usize = 100;
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginSource {
    Builtin,
    External(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    Running,
    Disabled,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub source: PluginSource,
    pub status: PluginStatus,
    pub enabled: bool,
}

/// Coalesced event-driven wake signal from external plugin reader threads to
/// the UI event loop (eliminates periodic polling).
#[derive(Clone)]
struct WakeSignal {
    tx: UnboundedSender<()>,
    pending: Arc<AtomicBool>,
}

impl WakeSignal {
    fn new() -> (Self, UnboundedReceiver<()>) {
        let (tx, rx) = unbounded();
        (
            Self {
                tx,
                pending: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.unbounded_send(());
        }
    }

    fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// Returns the user plugins folder (`IRIS_PLUGINS` override or `<data_dir>/plugins`).
pub fn plugins_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_PLUGINS") {
        return Some(PathBuf::from(p));
    }
    settings::data_dir().map(|d| d.join("plugins"))
}

/// Builds a read-only [`PlaybackSnapshot`] from the current playback session.
pub fn snapshot_from_session(session: &Session, favorite: bool) -> PlaybackSnapshot {
    let state = &session.state;
    let kind = session.kind().map(|k| match k {
        Kind::Video => MediaKind::Video,
        Kind::Photo => MediaKind::Photo,
        Kind::Audio => MediaKind::Audio,
    });
    let path = session
        .current
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    let file_name = session
        .current
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    PlaybackSnapshot {
        path,
        file_name,
        kind,
        position: state.time_pos,
        duration: state.duration,
        paused: state.paused,
        volume: state.volume,
        muted: state.muted,
        speed: state.speed,
        favorite,
        chapter: state.current_chapter,
    }
}

fn encode_event(event: &HostEvent) -> Result<Arc<String>, String> {
    let mut line = serde_json::to_string(&HostMessageRef::Event { payload: event })
        .map_err(|e| e.to_string())?;
    line.push('\n');
    Ok(Arc::new(line))
}

/// Reads newline-delimited lines with a strict byte bound even when the peer
/// never emits `\n` (prevents unbounded heap growth).
fn read_bounded_lines(
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
            Err(_) => break,
        };
        let nl = available.iter().position(|&b| b == b'\n');
        let count = nl.map_or(available.len(), |i| i + 1);
        if discarding {
            reader.consume(count);
            if nl.is_some() {
                discarding = false;
            }
            continue;
        }
        if buf.len() + count > max_bytes {
            buf.clear();
            on_oversized();
            reader.consume(count);
            if nl.is_none() {
                discarding = true;
            }
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

struct ExternalProcess {
    child: Child,
    stdin_tx: Option<SyncSender<Arc<String>>>,
    writer_thread: Option<JoinHandle<()>>,
    rx: Receiver<PluginMessage>,
    disconnected: bool,
}

impl ExternalProcess {
    fn spawn(
        dir: &Path,
        manifest: &PluginManifest,
        wake: Option<WakeSignal>,
    ) -> Result<Self, String> {
        let Some(entry) = manifest.entry.as_deref() else {
            return Err("manifest missing entry".into());
        };
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

        // Detect Python scripts by extension so cross-platform example plugins run seamlessly.
        let mut cmd = if canonical_entry
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("py"))
        {
            let python = if cfg!(target_os = "windows") {
                "python"
            } else {
                "python3"
            };
            let mut c = ProcessCommand::new(python);
            c.arg("-u").arg(&canonical_entry);
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
            // CREATE_NO_WINDOW prevents flashing a console window for GUI launches.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "missing stdin pipe".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing stdout pipe".to_string())?;
        let stderr = child.stderr.take();

        // Dedicated non-blocking writer thread: a stalled plugin can never block the UI thread.
        let (stdin_tx, stdin_rx) = mpsc::sync_channel::<Arc<String>>(MAX_STDIN_QUEUE);
        let plugin_id_w = manifest.id.clone();
        let writer_thread = std::thread::Builder::new()
            .name(format!("iris-plugin-w-{plugin_id_w}"))
            .spawn(move || {
                while let Ok(line) = stdin_rx.recv() {
                    if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
                        break;
                    }
                }
            })
            .ok();

        // Bounded stdout reader thread with event-driven wakeup.
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
                        if let Ok(msg) = serde_json::from_str::<PluginMessage>(trimmed) {
                            match tx.try_send(msg) {
                                Ok(()) => {
                                    if let Some(w) = &wake_out {
                                        w.notify();
                                    }
                                }
                                Err(TrySendError::Full(_)) => {
                                    log::warn!(
                                        "plugin {plugin_id} output queue full; dropping message"
                                    );
                                }
                                Err(TrySendError::Disconnected(_)) => return false,
                            }
                        }
                        true
                    },
                    || {
                        log::warn!(
                            "plugin {plugin_id} exceeded max line size ({MAX_LINE_BYTES} B)"
                        );
                    },
                );
                // Dropping `tx` signals disconnect; wake host so crash/exit status updates promptly.
                drop(tx);
                if let Some(w) = &wake_out {
                    w.notify();
                }
            });

        // Bounded stderr logger so plugin diagnostics reach iris.log without flooding it.
        if let Some(stderr) = stderr {
            let plugin_id_e = manifest.id.clone();
            let _ = std::thread::Builder::new()
                .name(format!("iris-plugin-e-{plugin_id_e}"))
                .spawn(move || {
                    let mut logged = 0usize;
                    read_bounded_lines(
                        stderr,
                        MAX_STDERR_LINE_BYTES,
                        |buf| {
                            if logged >= MAX_STDERR_LINES {
                                return false;
                            }
                            let text = String::from_utf8_lossy(buf);
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                log::warn!(
                                    target: "iris::plugin::stderr",
                                    "[{plugin_id_e}] {trimmed}"
                                );
                                logged += 1;
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

    fn send_encoded(&mut self, line: Arc<String>, droppable: bool) -> Result<(), String> {
        let Some(tx) = &self.stdin_tx else {
            return Err("plugin stdin closed".into());
        };
        match tx.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) if droppable => Ok(()),
            Err(TrySendError::Full(_)) => Err("plugin stdin queue full (unresponsive)".into()),
            Err(TrySendError::Disconnected(_)) => Err("plugin stdin closed".into()),
        }
    }

    fn send(&mut self, event: &HostEvent) -> Result<(), String> {
        let encoded = encode_event(event)?;
        let droppable = matches!(event, HostEvent::PositionTick { .. });
        self.send_encoded(encoded, droppable)
    }

    /// Drains up to `MAX_DRAIN_PER_POLL` messages. Returns `(commands, exited_reason)`.
    fn drain(&mut self) -> (Vec<HostCommand>, Option<String>) {
        let mut cmds = Vec::new();
        for _ in 0..MAX_DRAIN_PER_POLL {
            match self.rx.try_recv() {
                Ok(PluginMessage::Commands { commands }) => cmds.extend(commands),
                Ok(PluginMessage::Log { message }) => {
                    log::info!(target: "iris::plugin", "{message}");
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        let exited = if self.disconnected {
            let reason = match self.child.try_wait() {
                Ok(Some(status)) => format!("进程已退出 ({status})"),
                Ok(None) => "输出通道已关闭".into(),
                Err(e) => format!("进程状态异常: {e}"),
            };
            Some(reason)
        } else {
            None
        };
        (cmds, exited)
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stdin_tx.take()
            && let Ok(line) = encode_event(&HostEvent::Shutdown)
        {
            let _ = tx.try_send(line);
        }
        // Dropping `tx` above makes the writer close child stdin right after flushing `Shutdown`.
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

enum Runtime {
    Builtin(Box<dyn BuiltinPlugin>),
    External {
        dir: PathBuf,
        proc: Option<ExternalProcess>,
    },
}

struct LoadedPlugin {
    manifest: PluginManifest,
    source: PluginSource,
    status: PluginStatus,
    enabled: bool,
    runtime: Runtime,
    sidebar: Option<SidebarPanel>,
    status_item: Option<StatusItem>,
    overlay: Vec<OverlayShape>,
}

pub struct PluginManager {
    plugins: BTreeMap<String, LoadedPlugin>,
    host_version: String,
    wake: Option<WakeSignal>,
}

impl PluginManager {
    pub fn load(disabled: &[String]) -> Self {
        let mut mgr = Self {
            plugins: BTreeMap::new(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            wake: None,
        };
        mgr.reload(disabled);
        mgr
    }

    /// Loads plugins and returns an event-driven wake channel that fires only
    /// when an external plugin emits a message or exits (zero idle polling).
    pub fn load_with_wake(disabled: &[String]) -> (Self, UnboundedReceiver<()>) {
        let (wake, rx) = WakeSignal::new();
        let mut mgr = Self {
            plugins: BTreeMap::new(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            wake: Some(wake),
        };
        mgr.reload(disabled);
        (mgr, rx)
    }

    pub fn reload(&mut self, disabled: &[String]) {
        // Gracefully shut down existing plugins first.
        for plugin in self.plugins.values_mut() {
            if let Runtime::External { proc, .. } = &mut plugin.runtime {
                proc.take();
            }
        }
        self.plugins.clear();

        // 1. Register built-in official plugins.
        let builtins: Vec<Box<dyn BuiltinPlugin>> = vec![
            Box::new(GuidesPlugin::default()),
            Box::new(AbLoopPlugin::default()),
        ];
        for b in builtins {
            let manifest = b.manifest();
            let id = manifest.id.clone();
            let enabled = !disabled.iter().any(|d| d == &id);
            let mut loaded = LoadedPlugin {
                manifest,
                source: PluginSource::Builtin,
                status: if enabled {
                    PluginStatus::Running
                } else {
                    PluginStatus::Disabled
                },
                enabled,
                runtime: Runtime::Builtin(b),
                sidebar: None,
                status_item: None,
                overlay: Vec::new(),
            };
            if enabled {
                let init = HostEvent::Init {
                    protocol: PROTOCOL_VERSION,
                    host_version: self.host_version.clone(),
                };
                if let Runtime::Builtin(b) = &mut loaded.runtime {
                    let cmds = b.handle_event(&init);
                    let _ = Self::apply_plugin_commands(&mut loaded, cmds);
                }
            }
            self.plugins.insert(id, loaded);
        }

        // 2. Discover external plugins in `plugins_dir()`.
        if let Some(dir) = plugins_dir()
            && let Ok(read) = std::fs::read_dir(&dir)
        {
            for entry in read.flatten() {
                let pdir = entry.path();
                if !pdir.is_dir() {
                    continue;
                }
                let manifest_path = pdir.join("plugin.json");
                let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_str::<PluginManifest>(&text) else {
                    log::warn!("invalid plugin manifest: {}", manifest_path.display());
                    continue;
                };
                if let Err(reason) = manifest.validate() {
                    log::warn!("rejected plugin {}: {reason}", manifest_path.display());
                    continue;
                }
                let id = manifest.id.clone();
                if self.plugins.contains_key(&id) {
                    continue;
                }
                let enabled = !disabled.iter().any(|d| d == &id);
                let mut loaded = LoadedPlugin {
                    manifest,
                    source: PluginSource::External(pdir.clone()),
                    status: if enabled {
                        PluginStatus::Running
                    } else {
                        PluginStatus::Disabled
                    },
                    enabled,
                    runtime: Runtime::External {
                        dir: pdir,
                        proc: None,
                    },
                    sidebar: None,
                    status_item: None,
                    overlay: Vec::new(),
                };
                if enabled {
                    self.start_external(&mut loaded);
                }
                self.plugins.insert(id, loaded);
            }
        }
    }

    fn start_external(&self, loaded: &mut LoadedPlugin) {
        let Runtime::External { dir, proc } = &mut loaded.runtime else {
            return;
        };
        match ExternalProcess::spawn(dir, &loaded.manifest, self.wake.clone()) {
            Ok(mut p) => {
                let init = HostEvent::Init {
                    protocol: PROTOCOL_VERSION,
                    host_version: self.host_version.clone(),
                };
                if let Err(e) = p.send(&init) {
                    loaded.status = PluginStatus::Error(e);
                } else {
                    *proc = Some(p);
                    loaded.status = PluginStatus::Running;
                }
            }
            Err(e) => {
                loaded.status = PluginStatus::Error(e);
            }
        }
    }

    /// Toggles a plugin on or off at runtime.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        let host_version = self.host_version.clone();
        let wake = self.wake.clone();
        let Some(loaded) = self.plugins.get_mut(id) else {
            return;
        };
        if loaded.enabled == enabled {
            return;
        }
        loaded.enabled = enabled;
        if !enabled {
            loaded.status = PluginStatus::Disabled;
            loaded.sidebar = None;
            loaded.status_item = None;
            loaded.overlay.clear();
            if let Runtime::External { proc, .. } = &mut loaded.runtime {
                proc.take();
            }
            return;
        }
        let init = HostEvent::Init {
            protocol: PROTOCOL_VERSION,
            host_version,
        };
        match &mut loaded.runtime {
            Runtime::Builtin(b) => {
                loaded.status = PluginStatus::Running;
                let cmds = b.handle_event(&init);
                let _ = Self::apply_plugin_commands(loaded, cmds);
            }
            Runtime::External { dir, proc } => {
                match ExternalProcess::spawn(dir, &loaded.manifest, wake) {
                    Ok(mut p) => {
                        if let Err(e) = p.send(&init) {
                            loaded.status = PluginStatus::Error(e);
                        } else {
                            *proc = Some(p);
                            loaded.status = PluginStatus::Running;
                        }
                    }
                    Err(e) => {
                        loaded.status = PluginStatus::Error(e);
                    }
                }
            }
        }
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .values()
            .map(|p| PluginInfo {
                manifest: p.manifest.clone(),
                source: p.source.clone(),
                status: p.status.clone(),
                enabled: p.enabled,
            })
            .collect()
    }

    pub fn commands(&self) -> Vec<(&str, &str, &CommandSpec)> {
        let mut out = Vec::new();
        for p in self.plugins.values() {
            if !p.enabled || !matches!(p.status, PluginStatus::Running) {
                continue;
            }
            for cmd in &p.manifest.commands {
                out.push((p.manifest.id.as_str(), p.manifest.name.as_str(), cmd));
            }
        }
        out
    }

    pub fn sidebar_panels(&self) -> Vec<(&str, &SidebarPanel)> {
        self.plugins
            .values()
            .filter(|p| p.enabled)
            .filter_map(|p| p.sidebar.as_ref().map(|s| (p.manifest.id.as_str(), s)))
            .collect()
    }

    pub fn status_items(&self) -> Vec<(&str, &StatusItem)> {
        self.plugins
            .values()
            .filter(|p| p.enabled)
            .filter_map(|p| p.status_item.as_ref().map(|s| (p.manifest.id.as_str(), s)))
            .collect()
    }

    pub fn overlay_shapes(&self) -> Vec<&OverlayShape> {
        self.plugins
            .values()
            .filter(|p| p.enabled)
            .flat_map(|p| p.overlay.iter())
            .collect()
    }

    /// Broadcasts an event to all enabled plugins that hold `ReadState`.
    /// Serializes JSON at most once per broadcast for external plugins.
    pub fn dispatch(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        let mut host_cmds = Vec::new();
        let mut encoded: Option<Arc<String>> = None;
        let droppable = matches!(event, HostEvent::PositionTick { .. });
        for loaded in self.plugins.values_mut() {
            if !loaded.enabled || !loaded.manifest.has_permission(Permission::ReadState) {
                continue;
            }
            let emitted = match &mut loaded.runtime {
                Runtime::Builtin(b) => b.handle_event(event),
                Runtime::External { proc, .. } => {
                    if let Some(p) = proc.as_mut() {
                        let line = match &encoded {
                            Some(l) => Ok(l.clone()),
                            None => match encode_event(event) {
                                Ok(l) => {
                                    encoded = Some(l.clone());
                                    Ok(l)
                                }
                                Err(e) => Err(e),
                            },
                        };
                        match line.and_then(|l| p.send_encoded(l, droppable)) {
                            Ok(()) => {
                                let (cmds, exited) = p.drain();
                                if let Some(reason) = exited {
                                    loaded.status = PluginStatus::Error(reason);
                                    *proc = None;
                                }
                                cmds
                            }
                            Err(e) => {
                                loaded.status = PluginStatus::Error(e);
                                *proc = None;
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                }
            };
            host_cmds.extend(Self::apply_plugin_commands(loaded, emitted));
        }
        host_cmds
    }

    /// Drains asynchronous commands and exit notifications from external stdio
    /// plugins. Returns `(commands, changed)` where `changed` indicates UI slot
    /// or plugin status state changed (triggering a redraw).
    pub fn poll(&mut self) -> (Vec<HostCommand>, bool) {
        if let Some(wake) = &self.wake {
            wake.clear();
        }
        let mut host_cmds = Vec::new();
        let mut changed = false;
        for loaded in self.plugins.values_mut() {
            if !loaded.enabled {
                continue;
            }
            let (emitted, exited) = if let Runtime::External { proc, .. } = &mut loaded.runtime
                && let Some(p) = proc.as_mut()
            {
                let (emitted, exited) = p.drain();
                if exited.is_some() {
                    *proc = None;
                }
                (emitted, exited)
            } else {
                continue;
            };
            if !emitted.is_empty() {
                changed = true;
                host_cmds.extend(Self::apply_plugin_commands(loaded, emitted));
            }
            if let Some(reason) = exited {
                loaded.status = PluginStatus::Error(reason);
                loaded.sidebar = None;
                loaded.status_item = None;
                loaded.overlay.clear();
                changed = true;
            }
        }
        (host_cmds, changed)
    }

    pub fn invoke_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        snapshot: PlaybackSnapshot,
    ) -> Vec<HostCommand> {
        self.invoke_targeted(
            plugin_id,
            HostEvent::CommandInvoked {
                command_id: command_id.to_string(),
                snapshot,
            },
        )
    }

    pub fn invoke_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        snapshot: PlaybackSnapshot,
    ) -> Vec<HostCommand> {
        self.invoke_targeted(
            plugin_id,
            HostEvent::ActionInvoked {
                action_id: action_id.to_string(),
                snapshot,
            },
        )
    }

    fn invoke_targeted(&mut self, plugin_id: &str, event: HostEvent) -> Vec<HostCommand> {
        let Some(loaded) = self.plugins.get_mut(plugin_id) else {
            return Vec::new();
        };
        if !loaded.enabled {
            return Vec::new();
        }
        let emitted = match &mut loaded.runtime {
            Runtime::Builtin(b) => b.handle_event(&event),
            Runtime::External { proc, .. } => {
                if let Some(p) = proc.as_mut() {
                    if let Err(e) = p.send(&event) {
                        loaded.status = PluginStatus::Error(e);
                        *proc = None;
                        Vec::new()
                    } else {
                        let (cmds, exited) = p.drain();
                        if let Some(reason) = exited {
                            loaded.status = PluginStatus::Error(reason);
                            *proc = None;
                        }
                        cmds
                    }
                } else {
                    Vec::new()
                }
            }
        };
        Self::apply_plugin_commands(loaded, emitted)
    }

    /// Enforces manifest permissions, updates host-owned UI slots, and returns
    /// playback / toast commands for the window to execute.
    fn apply_plugin_commands(
        loaded: &mut LoadedPlugin,
        commands: Vec<HostCommand>,
    ) -> Vec<HostCommand> {
        let mut forward = Vec::new();
        for cmd in commands {
            let perm = cmd.required_permission();
            if !loaded.manifest.has_permission(perm) {
                log::warn!(
                    "plugin {} denied {:?} (missing {:?})",
                    loaded.manifest.id,
                    cmd,
                    perm
                );
                continue;
            }
            match cmd {
                HostCommand::SetSidebar { panel } => loaded.sidebar = panel,
                HostCommand::SetStatus { item } => loaded.status_item = item,
                HostCommand::SetOverlay { shapes } => loaded.overlay = shapes,
                HostCommand::ClearOverlay => loaded.overlay.clear(),
                other => forward.push(other),
            }
        }
        forward
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_api::OverlayColor;

    struct RoguePlugin;
    impl BuiltinPlugin for RoguePlugin {
        fn manifest(&self) -> PluginManifest {
            PluginManifest {
                id: "test.rogue".into(),
                name: "Rogue".into(),
                version: "0.1.0".into(),
                author: "Test".into(),
                description: "Requests only Notifications".into(),
                protocol: PROTOCOL_VERSION,
                entry: None,
                args: Vec::new(),
                permissions: vec![Permission::Notifications],
                commands: Vec::new(),
            }
        }
        fn handle_event(&mut self, _: &HostEvent) -> Vec<HostCommand> {
            vec![
                HostCommand::TogglePlayback,
                HostCommand::SetOverlay {
                    shapes: vec![OverlayShape::Grid {
                        cols: 3,
                        rows: 3,
                        color: OverlayColor::White,
                        opacity: 0.5,
                    }],
                },
                HostCommand::ShowToast {
                    message: "allowed".into(),
                },
            ]
        }
    }

    #[test]
    fn permissions_block_unauthorized_commands() {
        let mut loaded = LoadedPlugin {
            manifest: RoguePlugin.manifest(),
            source: PluginSource::Builtin,
            status: PluginStatus::Running,
            enabled: true,
            runtime: Runtime::Builtin(Box::new(RoguePlugin)),
            sidebar: None,
            status_item: None,
            overlay: Vec::new(),
        };
        let emitted = RoguePlugin.handle_event(&HostEvent::Shutdown);
        let allowed = PluginManager::apply_plugin_commands(&mut loaded, emitted);
        assert_eq!(
            allowed,
            vec![HostCommand::ShowToast {
                message: "allowed".into()
            }]
        );
        assert!(loaded.overlay.is_empty());
    }

    #[test]
    fn guides_plugin_contributes_overlay_and_status_slots() {
        let mut mgr = PluginManager {
            plugins: BTreeMap::new(),
            host_version: "0.1.0".into(),
            wake: None,
        };
        mgr.reload(&[]);
        assert!(mgr.overlay_shapes().is_empty());
        let cmds = mgr.invoke_command("iris.guides", "thirds", PlaybackSnapshot::default());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, HostCommand::ShowToast { .. }))
        );
        assert_eq!(mgr.overlay_shapes().len(), 1);
        assert_eq!(mgr.status_items().len(), 1);
        mgr.set_enabled("iris.guides", false);
        assert!(mgr.overlay_shapes().is_empty());
        assert!(mgr.status_items().is_empty());
    }

    #[test]
    fn ab_loop_plugin_loops_dedups_and_formats_hours() {
        assert_eq!(AbLoopPlugin::fmt_sec(90.), "01:30");
        assert_eq!(AbLoopPlugin::fmt_sec(3665.), "1:01:05");

        let mut mgr = PluginManager {
            plugins: BTreeMap::new(),
            host_version: "0.1.0".into(),
            wake: None,
        };
        mgr.reload(&[]);
        // Rejects photo media cleanly.
        let photo_snap = PlaybackSnapshot {
            kind: Some(MediaKind::Photo),
            ..Default::default()
        };
        let photo_cmds = mgr.invoke_command("iris.ab-loop", "set_a", photo_snap);
        assert!(
            photo_cmds.iter().any(
                |c| matches!(c, HostCommand::ShowToast { message } if message.contains("视频"))
            )
        );

        let mut snap = PlaybackSnapshot {
            path: Some("/demo/a.mp4".into()),
            kind: Some(MediaKind::Video),
            position: 5.0,
            duration: 60.0,
            ..Default::default()
        };
        mgr.dispatch(&HostEvent::FileOpened {
            snapshot: snap.clone(),
        });
        mgr.invoke_command("iris.ab-loop", "set_a", snap.clone());
        snap.position = 12.0;
        mgr.invoke_command("iris.ab-loop", "set_b", snap.clone());
        let tick_cmds = mgr.dispatch(&HostEvent::PositionTick {
            position: 12.1,
            duration: 60.0,
            paused: false,
        });
        assert_eq!(tick_cmds, vec![HostCommand::SeekAbsolute { seconds: 5.0 }]);
        // Immediate second tick within cooldown does not fire a duplicate seek.
        let dup_cmds = mgr.dispatch(&HostEvent::PositionTick {
            position: 12.2,
            duration: 60.0,
            paused: false,
        });
        assert!(dup_cmds.is_empty());

        mgr.invoke_command("iris.ab-loop", "add_marker", snap.clone());
        let jump = mgr.invoke_action("iris.ab-loop", "jump:12.000", snap);
        assert!(jump.contains(&HostCommand::SeekAbsolute { seconds: 12.0 }));
    }

    #[test]
    fn bounded_line_reader_skips_oversized_lines_without_allocating() {
        let mut input = vec![b'x'; 200];
        input.push(b'\n');
        input.extend_from_slice(b"good\n");
        let mut lines = Vec::new();
        let mut oversized = 0usize;
        read_bounded_lines(
            std::io::Cursor::new(input),
            64,
            |b| {
                lines.push(String::from_utf8_lossy(b).trim().to_string());
                true
            },
            || oversized += 1,
        );
        assert_eq!(oversized, 1);
        assert_eq!(lines, vec!["good"]);
    }

    #[test]
    fn external_stdio_example_plugin_handshake_and_command() {
        let example_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/watch-stats");
        let manifest_text = std::fs::read_to_string(example_dir.join("plugin.json")).unwrap();
        let manifest: PluginManifest = serde_json::from_str(&manifest_text).unwrap();
        assert!(manifest.validate().is_ok());

        let (wake, mut wake_rx) = WakeSignal::new();
        let Ok(mut proc) = ExternalProcess::spawn(&example_dir, &manifest, Some(wake)) else {
            return; // Skip if python3/python is unavailable in the test environment.
        };
        proc.send(&HostEvent::Init {
            protocol: PROTOCOL_VERSION,
            host_version: "0.1.0".into(),
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline && received.is_empty() {
            let (cmds, _) = proc.drain();
            received.extend(cmds);
            if received.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        // Wake signal fired upon plugin stdout message.
        assert!(wake_rx.try_recv().is_ok());
        assert!(
            received
                .iter()
                .any(|c| matches!(c, HostCommand::SetStatus { item: Some(_) })),
            "expected initial status item from watch-stats plugin, got {received:?}"
        );
        assert!(
            received
                .iter()
                .any(|c| matches!(c, HostCommand::SetSidebar { panel: Some(_) })),
            "expected initial sidebar panel from watch-stats plugin"
        );
        proc.stop();
    }

    #[test]
    fn external_plugin_crash_and_stalled_stdin_never_block_host() {
        let dir = std::env::temp_dir().join(format!("iris-plugin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Crashing plugin: exits immediately.
        let crash_script = dir.join("crash.py");
        std::fs::write(&crash_script, "import sys\nsys.exit(7)\n").unwrap();
        let crash_manifest = PluginManifest {
            id: "test.crash".into(),
            name: "Crash".into(),
            version: "0.1.0".into(),
            author: "Test".into(),
            description: String::new(),
            protocol: PROTOCOL_VERSION,
            entry: Some("crash.py".into()),
            args: Vec::new(),
            permissions: vec![Permission::ReadState],
            commands: Vec::new(),
        };
        let Ok(mut crash_proc) = ExternalProcess::spawn(&dir, &crash_manifest, None) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut detected = None;
        while Instant::now() < deadline && detected.is_none() {
            let (_, exited) = crash_proc.drain();
            if exited.is_some() {
                detected = exited;
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            detected.is_some(),
            "expected exit detection for crashed plugin"
        );
        crash_proc.stop();

        // 2. Stalled plugin: never reads stdin; flooding PositionTick must not block host.
        let stall_script = dir.join("stall.py");
        std::fs::write(&stall_script, "import time\ntime.sleep(10)\n").unwrap();
        let stall_manifest = PluginManifest {
            id: "test.stall".into(),
            name: "Stall".into(),
            version: "0.1.0".into(),
            author: "Test".into(),
            description: String::new(),
            protocol: PROTOCOL_VERSION,
            entry: Some("stall.py".into()),
            args: Vec::new(),
            permissions: vec![Permission::ReadState],
            commands: Vec::new(),
        };
        let mut stall_proc = ExternalProcess::spawn(&dir, &stall_manifest, None).unwrap();
        let started = Instant::now();
        let tick = HostEvent::PositionTick {
            position: 1.0,
            duration: 100.0,
            paused: false,
        };
        for _ in 0..500 {
            assert!(stall_proc.send(&tick).is_ok());
        }
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "sending 500 ticks to stalled plugin blocked for {:?}",
            started.elapsed()
        );
        stall_proc.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
