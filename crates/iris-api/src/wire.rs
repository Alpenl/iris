use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::manifest::Permission;
use crate::overlay::OverlayNode;
use crate::source::{
    PlaybackSnapshot, SourceDescriptor, SourceManageAction, SourcePage, StreamSpec,
};
use crate::workbench::{
    ChromeLayoutSpec, PluginModal, SidebarPanel, StatusBadge, ToolbarButton, UiThemeSpec,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HostEvent {
    /// Atomic startup handshake delivering protocol version, persisted non-secret
    /// `state`, encrypted `secrets` (when permitted), and initial `snapshot`.
    Init {
        protocol: u32,
        host_version: String,
        #[serde(default)]
        state: BTreeMap<String, String>,
        #[serde(default)]
        secrets: BTreeMap<String, String>,
        #[serde(default)]
        snapshot: PlaybackSnapshot,
    },
    MediaOpened {
        snapshot: PlaybackSnapshot,
    },
    StateChanged {
        snapshot: PlaybackSnapshot,
    },
    Tick {
        position: f64,
        duration: f64,
        paused: bool,
    },
    /// Command Palette or keyboard shortcut invocation.
    Command {
        id: String,
        snapshot: PlaybackSnapshot,
    },
    /// Unified UI interaction event from any sidebar/toolbar/status/overlay widget.
    /// `value` carries the new text/option/boolean/number for inputs, selects, toggles, and sliders.
    Action {
        id: String,
        #[serde(default)]
        value: Option<String>,
        snapshot: PlaybackSnapshot,
    },
    ModalSubmit {
        id: String,
        values: BTreeMap<String, String>,
    },
    ModalCancel {
        id: String,
    },
    SourceBrowse {
        request_id: String,
        source_id: String,
        #[serde(default)]
        folder_id: Option<String>,
        #[serde(default)]
        page: Option<String>,
        #[serde(default)]
        query: Option<String>,
    },
    SourceResolve {
        request_id: String,
        source_id: String,
        #[serde(default)]
        folder_id: Option<String>,
        entry_id: String,
        #[serde(default)]
        variant: Option<String>,
    },
    SourceManage {
        source_id: String,
        action: SourceManageAction,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum HostCommand {
    TogglePlayback,
    SeekRelative {
        seconds: f64,
    },
    SeekAbsolute {
        seconds: f64,
    },
    SetSpeed {
        speed: f64,
    },
    SetVolume {
        volume: f64,
    },
    ToggleMute,
    OpenNext,
    OpenPrevious,
    ToggleFavorite,
    ShowToast {
        message: String,
    },
    /// Persist a non-secret plugin preference key (`None` deletes) across restarts.
    SetState {
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Persist an encrypted credential (`None` deletes) in the OS Keychain / Credential Manager.
    SetSecret {
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    SetSidebar {
        #[serde(default)]
        panel: Option<Box<SidebarPanel>>,
    },
    SetStatus {
        #[serde(default)]
        badge: Option<Box<StatusBadge>>,
    },
    /// Replaces this plugin's viewport overlay layer (`nodes: []` clears it).
    SetOverlay {
        #[serde(default)]
        nodes: Vec<OverlayNode>,
    },
    SetToolbar {
        #[serde(default)]
        buttons: Vec<ToolbarButton>,
    },
    OpenModal {
        modal: Box<PluginModal>,
    },
    CloseModal {
        #[serde(default)]
        id: Option<String>,
    },
    SetTheme {
        #[serde(default)]
        theme: Option<Box<UiThemeSpec>>,
        #[serde(default)]
        chrome: Option<ChromeLayoutSpec>,
    },
    /// Dynamically replaces the list of virtual media sources exposed by this plugin.
    SetSources {
        #[serde(default)]
        sources: Vec<SourceDescriptor>,
    },
    ProvideSourcePage {
        request_id: String,
        page: Box<SourcePage>,
    },
    /// Fulfills a `SourceResolve` request (`request_id: Some(...)`) or proactively
    /// opens a stream (`request_id: None`).
    ResolveStream {
        #[serde(default)]
        request_id: Option<String>,
        stream: Box<StreamSpec>,
    },
    SourceError {
        request_id: String,
        message: String,
    },
}

impl HostCommand {
    /// Returns the permission required to execute this command, or `None` for
    /// baseline plugin-scoped state (`SetState`).
    pub fn required_permission(&self) -> Option<Permission> {
        Some(match self {
            HostCommand::SetState { .. } => return None,
            HostCommand::SetSecret { .. } => Permission::Secrets,
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
            HostCommand::SetOverlay { .. } => Permission::UiOverlay,
            HostCommand::SetToolbar { .. } => Permission::UiToolbar,
            HostCommand::OpenModal { .. } | HostCommand::CloseModal { .. } => Permission::UiModal,
            HostCommand::SetTheme { .. } => Permission::UiTheme,
            HostCommand::SetSources { .. }
            | HostCommand::ProvideSourcePage { .. }
            | HostCommand::ResolveStream { .. }
            | HostCommand::SourceError { .. } => Permission::MediaSource,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Event { payload: HostEvent },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessageRef<'a> {
    Event { payload: &'a HostEvent },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Commands { commands: Vec<HostCommand> },
    Log { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_command_is_compact_and_source_events_roundtrip() {
        assert!(
            std::mem::size_of::<HostCommand>() <= 64,
            "HostCommand size {} exceeded 64B",
            std::mem::size_of::<HostCommand>()
        );

        let legacy_resolve =
            r#"{"event":"source_resolve","request_id":"r1","source_id":"dav","entry_id":"/a.mkv"}"#;
        let ev: HostEvent = serde_json::from_str(legacy_resolve).unwrap();
        assert_eq!(
            ev,
            HostEvent::SourceResolve {
                request_id: "r1".into(),
                source_id: "dav".into(),
                folder_id: None,
                entry_id: "/a.mkv".into(),
                variant: None,
            }
        );

        let manage = HostEvent::SourceManage {
            source_id: "dav.nas".into(),
            action: SourceManageAction::Remove,
        };
        let encoded = serde_json::to_string(&manage).unwrap();
        assert_eq!(serde_json::from_str::<HostEvent>(&encoded).unwrap(), manage);
    }
}
