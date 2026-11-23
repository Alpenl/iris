//! Iris Plugin Protocol (`PROTOCOL_VERSION = 1`).
//!
//! Designed from first principles around four pillars:
//! 1. **Virtual Media Sources (`Permission::MediaSource`)**: Cloud drives, remote
//!    storage, live feeds, and custom video sources with dynamic multi-instance
//!    connections, folder hierarchy, search, pagination, multi-bitrate variants,
//!    external subtitles, and OS-keychain secret storage — validated by the host
//!    and streamed safely to the media engine.
//! 2. **Parameterized UI Workbench (`UiTheme`, `UiToolbar`, `UiSidebar`, `UiModal`,
//!    `UiStatus`, `UiOverlay`)**: Full runtime theme/metric/chrome control (including
//!    frameless/floating-dock layouts), unified interactive widgets across sidebars
//!    and modals, and viewport vector/HUD/waveform/text overlays.
//! 3. **Persistent State & Encrypted Secrets**: Non-secret plugin preferences (`state`)
//!    and OS-vault credentials (`secrets`) delivered atomically in `HostEvent::Init`.
//! 4. **Strict Pixel & Safety Isolation**: Plugins never touch decoded video/photo
//!    pixels and never inject raw media engine options or shell arguments.

pub mod manifest;
pub mod overlay;
pub mod source;
pub mod wire;
pub mod workbench;

pub use manifest::{CommandSpec, Permission, PluginManifest, SourceProviderSpec, is_valid_id};
pub use overlay::{OverlayNode, TextAlign};
pub use source::{
    MediaKind, PlaybackSnapshot, SourceDescriptor, SourceEntry, SourceEntryKind,
    SourceManageAction, SourcePage, StreamSpec, StreamVariant, SubtitleAttachment,
};
pub use wire::{HostCommand, HostEvent, HostMessage, HostMessageRef, PluginMessage};
pub use workbench::{
    ButtonSpec, ChromeLayoutSpec, Color, PluginModal, SelectOption, SidebarPanel, SidebarSection,
    StatusBadge, ToolbarButton, UiThemeSpec, Widget, is_valid_hex_color,
};

/// Single canonical JSON-lines stdio protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

pub type PluginResponder = std::sync::Arc<dyn Fn(Vec<HostCommand>) + Send + Sync>;

/// Trait implemented by built-in official plugins compiled into Iris.
pub trait BuiltinPlugin: Send {
    fn manifest(&self) -> PluginManifest;
    /// Optional async callback channel injected by the host so built-in plugins
    /// (e.g. `iris.webdav`) can offload network I/O to background threads without
    /// stalling the UI thread.
    fn bind_responder(&mut self, _responder: PluginResponder) {}
    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn wire_messages_and_flat_python_json_roundtrip_cleanly() {
        let payload = HostEvent::Action {
            id: "set_skin".into(),
            value: Some("neon".into()),
            snapshot: PlaybackSnapshot {
                uri: Some("file:///media/demo.mkv".into()),
                title: Some("demo.mkv".into()),
                kind: Some(MediaKind::Video),
                position: 12.5,
                duration: 120.0,
                volume: 80.0,
                speed: 1.0,
                favorite: true,
                ..Default::default()
            },
        };
        let ref_encoded =
            serde_json::to_string(&HostMessageRef::Event { payload: &payload }).unwrap();
        let host_msg = HostMessage::Event { payload };
        assert_eq!(ref_encoded, serde_json::to_string(&host_msg).unwrap());
        assert_eq!(
            serde_json::from_str::<HostMessage>(&ref_encoded).unwrap(),
            host_msg
        );

        // Flat JSON as emitted by external Python plugins (no extra Box wrapper layer).
        let flat_json = r##"{
            "type": "commands",
            "commands": [
                {"command": "set_sidebar", "panel": {"title": "Inspector", "sections": []}},
                {"command": "set_status", "badge": {"id": "net", "text": "DAV"}},
                {"command": "open_modal", "modal": {"id": "add_dav", "title": "Add WebDAV"}},
                {
                    "command": "set_theme",
                    "theme": {"name": "OLED", "window_bg": "#000000", "corner_radius": 8.0},
                    "chrome": {"hide_titlebar": true, "floating_dock": true, "hidden_dock_buttons": ["loop"]}
                },
                {
                    "command": "set_sources",
                    "sources": [{"id": "dav_1", "name": "NAS", "configurable": true, "removable": true}]
                },
                {
                    "command": "provide_source_page",
                    "request_id": "req-1",
                    "page": {"source_id": "dav_1", "title": "Movies"}
                },
                {
                    "command": "resolve_stream",
                    "request_id": "req-2",
                    "stream": {"url": "https://dav.local/film.mkv", "headers": {"Authorization": "Basic xxx"}}
                }
            ]
        }"##;

        let parsed: PluginMessage = serde_json::from_str(flat_json).unwrap();
        let reencoded = serde_json::to_string(&parsed).unwrap();
        let reparsed: PluginMessage = serde_json::from_str(&reencoded).unwrap();
        assert_eq!(parsed, reparsed);

        let PluginMessage::Commands { commands } = parsed else {
            panic!("expected commands message");
        };
        assert_eq!(commands.len(), 7);
        assert!(matches!(
            &commands[3],
            HostCommand::SetTheme {
                theme: Some(t),
                chrome: Some(c),
            } if t.name.as_deref() == Some("OLED")
                && t.corner_radius == Some(8.0)
                && c.hide_titlebar == Some(true)
                && c.hidden_dock_buttons == vec!["loop".to_string()]
        ));
    }

    #[test]
    fn all_host_commands_map_to_expected_permissions_and_responder_binds() {
        assert!(std::mem::size_of::<HostCommand>() <= 64);

        for cmd in [
            HostCommand::TogglePlayback,
            HostCommand::SeekRelative { seconds: 5.0 },
            HostCommand::SeekAbsolute { seconds: 10.0 },
            HostCommand::SetSpeed { speed: 1.5 },
            HostCommand::SetVolume { volume: 90.0 },
            HostCommand::ToggleMute,
            HostCommand::OpenNext,
            HostCommand::OpenPrevious,
            HostCommand::ToggleFavorite,
        ] {
            assert_eq!(cmd.required_permission(), Some(Permission::ControlPlayback));
        }

        let modal: PluginModal = serde_json::from_str(r#"{"id":"m","title":"M"}"#).unwrap();
        let page: SourcePage = serde_json::from_str(r#"{"source_id":"s","title":"T"}"#).unwrap();
        assert!(page.authenticated);
        let stream: StreamSpec =
            serde_json::from_str(r#"{"url":"https://example.com/a.mp4"}"#).unwrap();

        let cases: Vec<(HostCommand, Option<Permission>)> = vec![
            (
                HostCommand::SetState {
                    key: "k".into(),
                    value: None,
                },
                None,
            ),
            (
                HostCommand::SetSecret {
                    key: "s".into(),
                    value: None,
                },
                Some(Permission::Secrets),
            ),
            (
                HostCommand::ShowToast {
                    message: "hi".into(),
                },
                Some(Permission::Notifications),
            ),
            (
                HostCommand::SetSidebar { panel: None },
                Some(Permission::UiSidebar),
            ),
            (
                HostCommand::SetStatus { badge: None },
                Some(Permission::UiStatus),
            ),
            (
                HostCommand::SetOverlay { nodes: vec![] },
                Some(Permission::UiOverlay),
            ),
            (
                HostCommand::SetToolbar { buttons: vec![] },
                Some(Permission::UiToolbar),
            ),
            (
                HostCommand::OpenModal {
                    modal: Box::new(modal),
                },
                Some(Permission::UiModal),
            ),
            (
                HostCommand::CloseModal { id: None },
                Some(Permission::UiModal),
            ),
            (
                HostCommand::SetTheme {
                    theme: None,
                    chrome: None,
                },
                Some(Permission::UiTheme),
            ),
            (
                HostCommand::SetSources { sources: vec![] },
                Some(Permission::MediaSource),
            ),
            (
                HostCommand::ProvideSourcePage {
                    request_id: "r".into(),
                    page: Box::new(page),
                },
                Some(Permission::MediaSource),
            ),
            (
                HostCommand::ResolveStream {
                    request_id: None,
                    stream: Box::new(stream),
                },
                Some(Permission::MediaSource),
            ),
            (
                HostCommand::SourceError {
                    request_id: "r".into(),
                    message: "err".into(),
                },
                Some(Permission::MediaSource),
            ),
        ];

        for (cmd, expected) in cases {
            assert_eq!(cmd.required_permission(), expected, "mismatch for {cmd:?}");
        }

        struct DummyPlugin {
            responder: Option<PluginResponder>,
        }
        impl BuiltinPlugin for DummyPlugin {
            fn manifest(&self) -> PluginManifest {
                PluginManifest {
                    id: "iris.dummy".into(),
                    name: "Dummy".into(),
                    ..Default::default()
                }
            }
            fn bind_responder(&mut self, responder: PluginResponder) {
                self.responder = Some(responder);
            }
            fn handle_event(&mut self, _event: &HostEvent) -> Vec<HostCommand> {
                if let Some(cb) = &self.responder {
                    cb(vec![HostCommand::ShowToast {
                        message: "async".into(),
                    }]);
                }
                vec![]
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let mut plugin = DummyPlugin { responder: None };
        plugin.bind_responder(Arc::new(move |cmds| {
            sink.lock().unwrap().extend(cmds);
        }));
        let sync_cmds = plugin.handle_event(&HostEvent::Shutdown);
        assert!(sync_cmds.is_empty());
        assert_eq!(captured.lock().unwrap().len(), 1);
    }
}
