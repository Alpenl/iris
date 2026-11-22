//! Iris plugin protocol: control-plane events, semantic commands, and
//! structured declarative UI slots (sidebar, status bar, vector overlay,
//! command palette).
//!
//! Architectural invariants:
//! - Plugins never touch real-time pixel data; the pixel pipeline belongs to the core.
//! - Plugins never pass raw backend/mpv options or commands; every action is a
//!   typed [`HostCommand`] gated by explicit [`Permission`] declarations.
//! - The host owns the visual grammar and renders all slot contributions.

use serde::{Deserialize, Serialize};

/// Current JSON-lines stdio protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Capabilities a plugin must declare in its manifest before the host accepts
/// the corresponding commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Receive playback snapshots and position ticks.
    ReadState,
    /// Control transport, seek, speed, volume, favorites.
    ControlPlayback,
    /// Contribute structured sections into the sidebar plugin slot.
    UiSidebar,
    /// Contribute a compact badge into the control bar status slot.
    UiStatus,
    /// Draw normalized vector shapes over the viewport.
    UiOverlay,
    /// Show lightweight host toast notifications.
    Notifications,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Permission::ReadState => "读取状态",
            Permission::ControlPlayback => "播放控制",
            Permission::UiSidebar => "侧栏面板",
            Permission::UiStatus => "底栏状态",
            Permission::UiOverlay => "矢量叠层",
            Permission::Notifications => "提示消息",
        }
    }
}

/// A command contributed into the host Command Palette (`⌘P` / `Ctrl+P`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub shortcut_hint: Option<String>,
}

/// Static plugin metadata loaded from `plugin.json` (or returned by built-ins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_protocol")]
    pub protocol: u32,
    /// Executable or script relative to the plugin directory (external plugins only).
    #[serde(default)]
    pub entry: Option<String>,
    /// Optional command arguments passed before script/executable args.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
}

fn default_protocol() -> u32 {
    PROTOCOL_VERSION
}

impl PluginManifest {
    pub fn has_permission(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err("invalid plugin id");
        }
        if self.name.trim().is_empty() {
            return Err("empty plugin name");
        }
        if self.protocol != PROTOCOL_VERSION {
            return Err("unsupported protocol version");
        }
        if let Some(entry) = &self.entry {
            if entry.is_empty()
                || entry.contains(':')
                || entry.starts_with('/')
                || entry.starts_with('\\')
            {
                return Err("unsafe plugin entry path");
            }
            let mut has_component = false;
            for part in entry.split(['/', '\\']) {
                if part.is_empty() || part == "." || part == ".." {
                    return Err("unsafe plugin entry path");
                }
                has_component = true;
            }
            if !has_component {
                return Err("unsafe plugin entry path");
            }
            let path = std::path::Path::new(entry);
            if path.is_absolute()
                || !path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err("unsafe plugin entry path");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Photo,
    Audio,
}

/// Read-only playback snapshot delivered to plugins with [`Permission::ReadState`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    pub path: Option<String>,
    pub file_name: Option<String>,
    pub kind: Option<MediaKind>,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub favorite: bool,
    pub chapter: i64,
}

/// Events sent from the host to a plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HostEvent {
    Init {
        protocol: u32,
        host_version: String,
    },
    FileOpened {
        snapshot: PlaybackSnapshot,
    },
    StateChanged {
        snapshot: PlaybackSnapshot,
    },
    PositionTick {
        position: f64,
        duration: f64,
        paused: bool,
    },
    CommandInvoked {
        command_id: String,
        snapshot: PlaybackSnapshot,
    },
    ActionInvoked {
        action_id: String,
        snapshot: PlaybackSnapshot,
    },
    Shutdown,
}

// ---- Structured host-owned UI slots ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionButton {
    pub label: String,
    pub action_id: String,
    #[serde(default)]
    pub accent: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidebarItem {
    Text {
        text: String,
        #[serde(default)]
        muted: bool,
    },
    KeyValue {
        key: String,
        value: String,
        #[serde(default)]
        accent: bool,
    },
    Progress {
        label: String,
        ratio: f32,
    },
    Button {
        label: String,
        action_id: String,
        #[serde(default)]
        accent: bool,
    },
    ButtonRow {
        buttons: Vec<ActionButton>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidebarSection {
    pub heading: String,
    #[serde(default)]
    pub items: Vec<SidebarItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidebarPanel {
    pub title: String,
    #[serde(default)]
    pub sections: Vec<SidebarSection>,
}

/// Compact badge rendered inside the control bar status slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tooltip: Option<String>,
    #[serde(default)]
    pub accent: bool,
    #[serde(default)]
    pub action_id: Option<String>,
}

/// Palette-constrained vector overlay colours (host owns the visual grammar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayColor {
    #[default]
    White,
    Accent,
    Dim,
    Dark,
}

/// Normalized (`0.0..=1.0`) vector overlay primitives drawn over the viewport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum OverlayShape {
    /// Axis-aligned rectangle border in normalized viewport coordinates.
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        color: OverlayColor,
        #[serde(default = "default_opacity")]
        opacity: f32,
        #[serde(default = "default_stroke")]
        stroke: f32,
    },
    /// Horizontal or vertical guide line (`x1 == x2` or `y1 == y2` renders crisp 1px).
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        #[serde(default)]
        color: OverlayColor,
        #[serde(default = "default_opacity")]
        opacity: f32,
        #[serde(default = "default_stroke")]
        thickness: f32,
    },
    /// Evenly spaced `cols × rows` grid across the viewport (e.g. 3×3 rule of thirds).
    Grid {
        cols: u8,
        rows: u8,
        #[serde(default)]
        color: OverlayColor,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Centered crosshair at `(x, y)` with arm length `size` (fraction of viewport).
    Crosshair {
        x: f32,
        y: f32,
        size: f32,
        #[serde(default)]
        color: OverlayColor,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Compact mono label anchored at `(x, y)`.
    Label {
        x: f32,
        y: f32,
        text: String,
        #[serde(default)]
        color: OverlayColor,
    },
}

fn default_opacity() -> f32 {
    0.55
}

fn default_stroke() -> f32 {
    1.0
}

/// Safe semantic commands emitted by a plugin back to the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum HostCommand {
    TogglePlayback,
    SeekRelative { seconds: f64 },
    SeekAbsolute { seconds: f64 },
    SetSpeed { speed: f64 },
    SetVolume { volume: f64 },
    ToggleMute,
    OpenNext,
    OpenPrevious,
    ToggleFavorite,
    ShowToast { message: String },
    SetSidebar { panel: Option<SidebarPanel> },
    SetStatus { item: Option<StatusItem> },
    SetOverlay { shapes: Vec<OverlayShape> },
    ClearOverlay,
}

impl HostCommand {
    /// Returns the capability required to execute this command.
    pub fn required_permission(&self) -> Permission {
        match self {
            HostCommand::TogglePlayback
            | HostCommand::SeekRelative { .. }
            | HostCommand::SeekAbsolute { .. }
            | HostCommand::SetSpeed { .. }
            | HostCommand::SetVolume { .. }
            | HostCommand::ToggleMute
            | HostCommand::OpenNext
            | HostCommand::OpenPrevious
            | HostCommand::ToggleFavorite => Permission::ControlPlayback,
            HostCommand::ShowToast { .. } => Permission::Notifications,
            HostCommand::SetSidebar { .. } => Permission::UiSidebar,
            HostCommand::SetStatus { .. } => Permission::UiStatus,
            HostCommand::SetOverlay { .. } | HostCommand::ClearOverlay => Permission::UiOverlay,
        }
    }
}

/// Wire envelope from host to external plugin (newline-delimited JSON on stdin).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Event { payload: HostEvent },
}

/// Borrowed wire envelope for zero-clone serialization of [`HostEvent`].
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessageRef<'a> {
    Event { payload: &'a HostEvent },
}

/// Wire envelope from external plugin to host (newline-delimited JSON on stdout).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Commands { commands: Vec<HostCommand> },
    Log { message: String },
}

/// Trait implemented by built-in official plugins compiled into Iris.
/// Uses the exact same control-plane events and commands as external plugins.
pub trait BuiltinPlugin: Send {
    fn manifest(&self) -> PluginManifest;
    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_messages_roundtrip_json() {
        let host_msg = HostMessage::Event {
            payload: HostEvent::CommandInvoked {
                command_id: "toggle".into(),
                snapshot: PlaybackSnapshot {
                    path: Some("/media/demo.mkv".into()),
                    file_name: Some("demo.mkv".into()),
                    kind: Some(MediaKind::Video),
                    position: 12.5,
                    duration: 120.0,
                    paused: false,
                    volume: 80.0,
                    muted: false,
                    speed: 1.0,
                    favorite: true,
                    chapter: 0,
                },
            },
        };
        let encoded = serde_json::to_string(&host_msg).unwrap();
        let decoded: HostMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, host_msg);

        let plugin_msg = PluginMessage::Commands {
            commands: vec![
                HostCommand::ShowToast {
                    message: "ok".into(),
                },
                HostCommand::SetOverlay {
                    shapes: vec![OverlayShape::Grid {
                        cols: 3,
                        rows: 3,
                        color: OverlayColor::White,
                        opacity: 0.4,
                    }],
                },
            ],
        };
        let encoded = serde_json::to_string(&plugin_msg).unwrap();
        let decoded: PluginMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, plugin_msg);
    }

    #[test]
    fn manifest_rejects_traversal_and_bad_protocol() {
        let mut m = PluginManifest {
            id: "acme.stats".into(),
            name: "Stats".into(),
            version: "0.1.0".into(),
            author: "Acme".into(),
            description: "Watch timer".into(),
            protocol: PROTOCOL_VERSION,
            entry: Some("main.py".into()),
            args: Vec::new(),
            permissions: vec![Permission::ReadState],
            commands: Vec::new(),
        };
        assert!(m.validate().is_ok());
        for bad in [
            "",
            "../escape.py",
            "sub/../escape.py",
            "/abs/path.py",
            r"\abs\path.py",
            "C:relative.exe",
            r"C:\abs.exe",
            "./dot.py",
            "a//b.py",
        ] {
            m.entry = Some(bad.into());
            assert!(m.validate().is_err(), "expected rejection for {bad:?}");
        }
        m.entry = Some("bin/plugin.py".into());
        assert!(m.validate().is_ok());
        m.entry = Some("main.py".into());
        m.protocol = 99;
        assert!(m.validate().is_err());

        let ev = HostEvent::Shutdown;
        let owned = serde_json::to_string(&HostMessage::Event {
            payload: ev.clone(),
        })
        .unwrap();
        let borrowed = serde_json::to_string(&HostMessageRef::Event { payload: &ev }).unwrap();
        assert_eq!(owned, borrowed);
    }
}
