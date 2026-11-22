//! Control-plane & vector-overlay plugin manager.
//!
//! Manages both built-in official plugins and external out-of-process plugins
//! discovered under `<app_data_dir>/plugins/<id>/plugin.json` (or `IRIS_PLUGINS`).
//! External plugins communicate via newline-delimited JSON over stdio using
//! [`iris_api::HostMessage`] and [`iris_api::PluginMessage`].

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver};

use iris_api::{
    ActionButton, BuiltinPlugin, CommandSpec, HostCommand, HostEvent, HostMessage, MediaKind,
    OverlayColor, OverlayShape, PROTOCOL_VERSION, Permission, PlaybackSnapshot, PluginManifest,
    PluginMessage, SidebarItem, SidebarPanel, SidebarSection, StatusItem,
};

use crate::library::Kind;
use crate::player::Session;
use crate::settings;

/// Maximum JSON line accepted from an external plugin (64 KiB).
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Maximum queued plugin messages drained in a single host poll.
const MAX_DRAIN_PER_POLL: usize = 256;

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

struct ExternalProcess {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<PluginMessage>,
}

impl ExternalProcess {
    fn spawn(dir: &Path, manifest: &PluginManifest) -> Result<Self, String> {
        let Some(entry) = manifest.entry.as_deref() else {
            return Err("manifest missing entry".into());
        };
        let entry_path = dir.join(entry);
        if !entry_path.exists() {
            return Err(format!("entry not found: {}", entry_path.display()));
        }
        // Detect Python scripts by extension so cross-platform example plugins run seamlessly.
        let mut cmd = if entry_path
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
            c.arg("-u").arg(&entry_path);
            c
        } else {
            ProcessCommand::new(&entry_path)
        };
        cmd.args(&manifest.args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW prevents flashing a console window for GUI launches.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "missing stdin pipe".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing stdout pipe".to_string())?;
        let (tx, rx) = mpsc::channel();
        let plugin_id = manifest.id.clone();
        let _ = std::thread::Builder::new()
            .name(format!("iris-plugin-{plugin_id}"))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::with_capacity(1024);
                loop {
                    buf.clear();
                    let read = reader.read_until(b'\n', &mut buf);
                    let Ok(n) = read else { break };
                    if n == 0 {
                        break;
                    }
                    if buf.len() > MAX_LINE_BYTES {
                        log::warn!("plugin {plugin_id} exceeded max line size");
                        continue;
                    }
                    if let Ok(line) = std::str::from_utf8(&buf)
                        && let Ok(msg) = serde_json::from_str::<PluginMessage>(line.trim())
                        && tx.send(msg).is_err()
                    {
                        break;
                    }
                }
            });
        Ok(Self { child, stdin, rx })
    }

    fn send(&mut self, event: &HostEvent) -> Result<(), String> {
        let msg = HostMessage::Event {
            payload: event.clone(),
        };
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| e.to_string())
    }

    fn drain(&mut self) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        for _ in 0..MAX_DRAIN_PER_POLL {
            match self.rx.try_recv() {
                Ok(PluginMessage::Commands { commands }) => cmds.extend(commands),
                Ok(PluginMessage::Log { message }) => {
                    log::info!(target: "iris::plugin", "{message}");
                }
                Err(_) => break,
            }
        }
        cmds
    }

    fn stop(&mut self) {
        let _ = self.send(&HostEvent::Shutdown);
        let _ = self.child.kill();
        let _ = self.child.wait();
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
}

impl PluginManager {
    pub fn load(disabled: &[String]) -> Self {
        let mut mgr = Self {
            plugins: BTreeMap::new(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        mgr.reload(disabled);
        mgr
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
        match ExternalProcess::spawn(dir, &loaded.manifest) {
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
            Runtime::External { dir, proc } => match ExternalProcess::spawn(dir, &loaded.manifest) {
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
            },
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
    pub fn dispatch(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        let mut host_cmds = Vec::new();
        for loaded in self.plugins.values_mut() {
            if !loaded.enabled || !loaded.manifest.has_permission(Permission::ReadState) {
                continue;
            }
            let emitted = match &mut loaded.runtime {
                Runtime::Builtin(b) => b.handle_event(event),
                Runtime::External { proc, .. } => {
                    if let Some(p) = proc.as_mut() {
                        if let Err(e) = p.send(event) {
                            loaded.status = PluginStatus::Error(e);
                            *proc = None;
                            Vec::new()
                        } else {
                            p.drain()
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

    /// Polls asynchronous commands from external stdio plugins.
    pub fn poll(&mut self) -> Vec<HostCommand> {
        let mut host_cmds = Vec::new();
        for loaded in self.plugins.values_mut() {
            if !loaded.enabled {
                continue;
            }
            if let Runtime::External {
                proc: Some(p), ..
            } = &mut loaded.runtime
            {
                let emitted = p.drain();
                if !emitted.is_empty() {
                    host_cmds.extend(Self::apply_plugin_commands(loaded, emitted));
                }
            }
        }
        host_cmds
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
                        p.drain()
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

// ============================================================================
// Built-in Official Plugin 1: Composition & Safe-Area Vector Guides
// ============================================================================

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum GuideMode {
    #[default]
    Off,
    RuleOfThirds,
    GoldenRatio,
    Crosshair,
    Cinema239,
    ActionSafe,
}

impl GuideMode {
    fn next(self) -> Self {
        match self {
            GuideMode::Off => GuideMode::RuleOfThirds,
            GuideMode::RuleOfThirds => GuideMode::GoldenRatio,
            GuideMode::GoldenRatio => GuideMode::Crosshair,
            GuideMode::Crosshair => GuideMode::Cinema239,
            GuideMode::Cinema239 => GuideMode::ActionSafe,
            GuideMode::ActionSafe => GuideMode::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            GuideMode::Off => "关闭",
            GuideMode::RuleOfThirds => "三分线",
            GuideMode::GoldenRatio => "黄金分割",
            GuideMode::Crosshair => "中心十字",
            GuideMode::Cinema239 => "2.39:1 参考框",
            GuideMode::ActionSafe => "安全区 90/80%",
        }
    }

    fn shapes(self) -> Vec<OverlayShape> {
        match self {
            GuideMode::Off => Vec::new(),
            GuideMode::RuleOfThirds => vec![OverlayShape::Grid {
                cols: 3,
                rows: 3,
                color: OverlayColor::White,
                opacity: 0.45,
            }],
            GuideMode::GoldenRatio => vec![
                OverlayShape::Line {
                    x1: 0.382,
                    y1: 0.,
                    x2: 0.382,
                    y2: 1.,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.618,
                    y1: 0.,
                    x2: 0.618,
                    y2: 1.,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.,
                    y1: 0.382,
                    x2: 1.,
                    y2: 0.382,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.,
                    y1: 0.618,
                    x2: 1.,
                    y2: 0.618,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
            ],
            GuideMode::Crosshair => vec![OverlayShape::Crosshair {
                x: 0.5,
                y: 0.5,
                size: 0.08,
                color: OverlayColor::Accent,
                opacity: 0.85,
            }],
            GuideMode::Cinema239 => {
                // 2.39:1 frame centered inside a 16:9 viewport (~12.8% matte top/bottom).
                vec![
                    OverlayShape::Rect {
                        x: 0.0,
                        y: 0.128,
                        w: 1.0,
                        h: 0.744,
                        color: OverlayColor::Accent,
                        opacity: 0.75,
                        stroke: 1.0,
                    },
                    OverlayShape::Label {
                        x: 0.015,
                        y: 0.14,
                        text: "2.39:1 SCOPE".into(),
                        color: OverlayColor::Accent,
                    },
                ]
            }
            GuideMode::ActionSafe => vec![
                OverlayShape::Rect {
                    x: 0.05,
                    y: 0.05,
                    w: 0.90,
                    h: 0.90,
                    color: OverlayColor::White,
                    opacity: 0.5,
                    stroke: 1.0,
                },
                OverlayShape::Rect {
                    x: 0.10,
                    y: 0.10,
                    w: 0.80,
                    h: 0.80,
                    color: OverlayColor::Dim,
                    opacity: 0.45,
                    stroke: 1.0,
                },
                OverlayShape::Label {
                    x: 0.06,
                    y: 0.06,
                    text: "SAFE 90% / 80%".into(),
                    color: OverlayColor::White,
                },
            ],
        }
    }
}

#[derive(Default)]
struct GuidesPlugin {
    mode: GuideMode,
}

impl GuidesPlugin {
    fn sync_ui(&self, announce: bool) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let shapes = self.mode.shapes();
        if shapes.is_empty() {
            cmds.push(HostCommand::ClearOverlay);
            cmds.push(HostCommand::SetStatus { item: None });
        } else {
            cmds.push(HostCommand::SetOverlay { shapes });
            cmds.push(HostCommand::SetStatus {
                item: Some(StatusItem {
                    id: "guide-badge".into(),
                    text: self.mode.label().into(),
                    tooltip: Some("构图辅助线 · 点击切换".into()),
                    accent: true,
                    action_id: Some("cycle".into()),
                }),
            });
        }
        cmds.push(HostCommand::SetSidebar {
            panel: Some(SidebarPanel {
                title: "构图辅助线".into(),
                sections: vec![SidebarSection {
                    heading: "矢量参考线（仅叠加，不改像素）".into(),
                    items: vec![
                        SidebarItem::KeyValue {
                            key: "当前模式".into(),
                            value: self.mode.label().into(),
                            accent: self.mode != GuideMode::Off,
                        },
                        SidebarItem::ButtonRow {
                            buttons: vec![
                                ActionButton {
                                    label: "关闭".into(),
                                    action_id: "off".into(),
                                    accent: self.mode == GuideMode::Off,
                                },
                                ActionButton {
                                    label: "三分线".into(),
                                    action_id: "thirds".into(),
                                    accent: self.mode == GuideMode::RuleOfThirds,
                                },
                                ActionButton {
                                    label: "黄金分割".into(),
                                    action_id: "golden".into(),
                                    accent: self.mode == GuideMode::GoldenRatio,
                                },
                            ],
                        },
                        SidebarItem::ButtonRow {
                            buttons: vec![
                                ActionButton {
                                    label: "十字".into(),
                                    action_id: "cross".into(),
                                    accent: self.mode == GuideMode::Crosshair,
                                },
                                ActionButton {
                                    label: "2.39:1".into(),
                                    action_id: "cinema".into(),
                                    accent: self.mode == GuideMode::Cinema239,
                                },
                                ActionButton {
                                    label: "安全区".into(),
                                    action_id: "safe".into(),
                                    accent: self.mode == GuideMode::ActionSafe,
                                },
                            ],
                        },
                    ],
                }],
            }),
        });
        if announce {
            cmds.push(HostCommand::ShowToast {
                message: format!("构图辅助线 · {}", self.mode.label()),
            });
        }
        cmds
    }

    fn apply_action(&mut self, action: &str) -> Vec<HostCommand> {
        self.mode = match action {
            "cycle" => self.mode.next(),
            "thirds" => GuideMode::RuleOfThirds,
            "golden" => GuideMode::GoldenRatio,
            "cross" => GuideMode::Crosshair,
            "cinema" => GuideMode::Cinema239,
            "safe" => GuideMode::ActionSafe,
            "off" => GuideMode::Off,
            _ => return Vec::new(),
        };
        self.sync_ui(true)
    }
}

impl BuiltinPlugin for GuidesPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.guides".into(),
            name: "构图辅助线".into(),
            version: "0.1.0".into(),
            author: "Iris".into(),
            description: "在视口矢量叠加层绘制三分线、黄金分割、中心十字、2.39:1 电影画幅与安全框参考线".into(),
            protocol: PROTOCOL_VERSION,
            entry: None,
            args: Vec::new(),
            permissions: vec![
                Permission::ReadState,
                Permission::UiOverlay,
                Permission::UiStatus,
                Permission::UiSidebar,
                Permission::Notifications,
            ],
            commands: vec![
                CommandSpec {
                    id: "cycle".into(),
                    title: "构图：切换辅助线模式".into(),
                    subtitle: Some("三分线 / 黄金分割 / 十字 / 2.39:1 / 安全区".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "thirds".into(),
                    title: "构图：显示三分线 (3×3)".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "golden".into(),
                    title: "构图：显示黄金分割线".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "cinema".into(),
                    title: "构图：显示 2.39:1 电影画幅框".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "off".into(),
                    title: "构图：清除辅助线".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
            ],
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { .. } => self.sync_ui(false),
            HostEvent::CommandInvoked { command_id, .. } => self.apply_action(command_id),
            HostEvent::ActionInvoked { action_id, .. } => self.apply_action(action_id),
            _ => Vec::new(),
        }
    }
}

// ============================================================================
// Built-in Official Plugin 2: A-B Loop & Review Markers
// ============================================================================

#[derive(Default)]
struct AbLoopPlugin {
    current_path: Option<String>,
    point_a: Option<f64>,
    point_b: Option<f64>,
    markers: BTreeMap<String, Vec<f64>>,
}

impl AbLoopPlugin {
    fn fmt_sec(t: f64) -> String {
        let s = t.max(0.) as u64;
        format!("{:02}:{:02}", s / 60, s % 60)
    }

    fn sync_ui(&self) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let status = match (self.point_a, self.point_b) {
            (Some(a), Some(b)) => Some(StatusItem {
                id: "ab-status".into(),
                text: format!("A-B {}–{}", Self::fmt_sec(a), Self::fmt_sec(b)),
                tooltip: Some("A-B 区间循环中 · 点击清除".into()),
                accent: true,
                action_id: Some("clear".into()),
            }),
            (Some(a), None) => Some(StatusItem {
                id: "ab-status".into(),
                text: format!("A {}–?", Self::fmt_sec(a)),
                tooltip: Some("已设置起点 A · 点击清除".into()),
                accent: false,
                action_id: Some("clear".into()),
            }),
            _ => None,
        };
        cmds.push(HostCommand::SetStatus { item: status });

        let mut sections = vec![SidebarSection {
            heading: "A-B 区间循环".into(),
            items: vec![
                SidebarItem::KeyValue {
                    key: "起点 A".into(),
                    value: self
                        .point_a
                        .map_or("未设置".into(), Self::fmt_sec),
                    accent: self.point_a.is_some(),
                },
                SidebarItem::KeyValue {
                    key: "终点 B".into(),
                    value: self
                        .point_b
                        .map_or("未设置".into(), Self::fmt_sec),
                    accent: self.point_b.is_some(),
                },
                SidebarItem::ButtonRow {
                    buttons: vec![
                        ActionButton {
                            label: "设起点 A".into(),
                            action_id: "set_a".into(),
                            accent: false,
                        },
                        ActionButton {
                            label: "设终点 B".into(),
                            action_id: "set_b".into(),
                            accent: false,
                        },
                        ActionButton {
                            label: "清除".into(),
                            action_id: "clear".into(),
                            accent: false,
                        },
                    ],
                },
            ],
        }];

        let mut marker_items = vec![SidebarItem::Button {
            label: "+ 在当前时间添加标记".into(),
            action_id: "add_marker".into(),
            accent: false,
        }];
        if let Some(path) = &self.current_path
            && let Some(list) = self.markers.get(path)
        {
            for (i, &t) in list.iter().enumerate() {
                marker_items.push(SidebarItem::Button {
                    label: format!("#{}  {}", i + 1, Self::fmt_sec(t)),
                    action_id: format!("jump:{t:.3}"),
                    accent: false,
                });
            }
        }
        sections.push(SidebarSection {
            heading: "时间点标记".into(),
            items: marker_items,
        });

        cmds.push(HostCommand::SetSidebar {
            panel: Some(SidebarPanel {
                title: "A-B 循环与标记".into(),
                sections,
            }),
        });
        cmds
    }

    fn run_action(&mut self, action: &str, snapshot: &PlaybackSnapshot) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        if let Some(ts) = action.strip_prefix("jump:")
            && let Ok(sec) = ts.parse::<f64>()
        {
            cmds.push(HostCommand::SeekAbsolute { seconds: sec });
            cmds.push(HostCommand::ShowToast {
                message: format!("跳转标记 · {}", Self::fmt_sec(sec)),
            });
            return cmds;
        }
        match action {
            "set_a" => {
                self.point_a = Some(snapshot.position.max(0.));
                if let Some(b) = self.point_b
                    && b <= snapshot.position
                {
                    self.point_b = None;
                }
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: format!("A-B 起点 A · {}", Self::fmt_sec(snapshot.position)),
                });
            }
            "set_b" => {
                let pos = snapshot.position.max(0.);
                match self.point_a {
                    Some(a) if pos > a + 0.2 => {
                        self.point_b = Some(pos);
                        cmds.extend(self.sync_ui());
                        cmds.push(HostCommand::ShowToast {
                            message: format!(
                                "A-B 循环 · {}–{}",
                                Self::fmt_sec(a),
                                Self::fmt_sec(pos)
                            ),
                        });
                    }
                    _ => {
                        cmds.push(HostCommand::ShowToast {
                            message: "请先设置起点 A，并确保终点 B 在 A 之后".into(),
                        });
                    }
                }
            }
            "clear" => {
                self.point_a = None;
                self.point_b = None;
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: "已清除 A-B 循环".into(),
                });
            }
            "add_marker" => {
                if let Some(path) = snapshot.path.clone() {
                    let list = self.markers.entry(path).or_default();
                    let t = snapshot.position.max(0.);
                    list.push(t);
                    list.sort_by(f64::total_cmp);
                    cmds.extend(self.sync_ui());
                    cmds.push(HostCommand::ShowToast {
                        message: format!("已添加时间标记 · {}", Self::fmt_sec(t)),
                    });
                }
            }
            _ => {}
        }
        cmds
    }
}

impl BuiltinPlugin for AbLoopPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.ab-loop".into(),
            name: "A-B 循环与标记".into(),
            version: "0.1.0".into(),
            author: "Iris".into(),
            description: "设置视频 A-B 区间自动循环播放，并记录当前文件的时间点书签以便审片跳转".into(),
            protocol: PROTOCOL_VERSION,
            entry: None,
            args: Vec::new(),
            permissions: vec![
                Permission::ReadState,
                Permission::ControlPlayback,
                Permission::UiSidebar,
                Permission::UiStatus,
                Permission::Notifications,
            ],
            commands: vec![
                CommandSpec {
                    id: "set_a".into(),
                    title: "A-B 循环：设置起点 A".into(),
                    subtitle: Some("以当前播放时间作为循环起点".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "set_b".into(),
                    title: "A-B 循环：设置终点 B".into(),
                    subtitle: Some("到达 B 点后自动跳回 A 点".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "clear".into(),
                    title: "A-B 循环：清除区间".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "add_marker".into(),
                    title: "标记：在当前时间添加书签".into(),
                    subtitle: Some("在侧栏插件面板点击即可跳转".into()),
                    shortcut_hint: None,
                },
            ],
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { .. } => self.sync_ui(),
            HostEvent::FileOpened { snapshot } => {
                self.current_path = snapshot.path.clone();
                self.point_a = None;
                self.point_b = None;
                self.sync_ui()
            }
            HostEvent::PositionTick {
                position, paused, ..
            } => {
                if !paused
                    && let (Some(a), Some(b)) = (self.point_a, self.point_b)
                    && *position >= b
                {
                    return vec![HostCommand::SeekAbsolute { seconds: a }];
                }
                Vec::new()
            }
            HostEvent::CommandInvoked {
                command_id,
                snapshot,
            } => self.run_action(command_id, snapshot),
            HostEvent::ActionInvoked {
                action_id,
                snapshot,
            } => self.run_action(action_id, snapshot),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        mgr.reload(&[]);
        assert!(mgr.overlay_shapes().is_empty());
        let cmds = mgr.invoke_command("iris.guides", "thirds", PlaybackSnapshot::default());
        assert!(cmds.iter().any(|c| matches!(c, HostCommand::ShowToast { .. })));
        assert_eq!(mgr.overlay_shapes().len(), 1);
        assert_eq!(mgr.status_items().len(), 1);
        mgr.set_enabled("iris.guides", false);
        assert!(mgr.overlay_shapes().is_empty());
        assert!(mgr.status_items().is_empty());
    }

    #[test]
    fn ab_loop_plugin_loops_and_jumps_to_markers() {
        let mut mgr = PluginManager {
            plugins: BTreeMap::new(),
            host_version: "0.1.0".into(),
        };
        mgr.reload(&[]);
        let mut snap = PlaybackSnapshot {
            path: Some("/demo/a.mp4".into()),
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

        mgr.invoke_command("iris.ab-loop", "add_marker", snap.clone());
        let jump = mgr.invoke_action("iris.ab-loop", "jump:12.000", snap);
        assert!(jump.contains(&HostCommand::SeekAbsolute { seconds: 12.0 }));
    }

    #[test]
    fn external_stdio_example_plugin_handshake_and_command() {
        let example_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/plugins/watch-stats");
        let manifest_text = std::fs::read_to_string(example_dir.join("plugin.json")).unwrap();
        let manifest: PluginManifest = serde_json::from_str(&manifest_text).unwrap();
        assert!(manifest.validate().is_ok());

        let Ok(mut proc) = ExternalProcess::spawn(&example_dir, &manifest) else {
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
            received.extend(proc.drain());
            if received.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
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
}
