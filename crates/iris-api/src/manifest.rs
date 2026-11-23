use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;
use crate::source::SourceDescriptor;
use crate::workbench::{ChromeLayoutSpec, UiThemeSpec};

/// Capabilities a plugin declares in `plugin.json`. Every host command and
/// sensitive event is gated by its required permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Receive playback snapshots and position ticks.
    ReadState,
    /// Control transport, seek, speed, volume, and favorites.
    ControlPlayback,
    /// Render interactive panels in the sidebar.
    UiSidebar,
    /// Render a compact badge in the control bar status slot.
    UiStatus,
    /// Draw vector primitives, text, waveforms, and HUD buttons over the viewport.
    UiOverlay,
    /// Register custom action buttons on the control bar.
    UiToolbar,
    /// Open interactive configuration, OAuth, and QR-login modals.
    UiModal,
    /// Override runtime colour tokens, layout metrics, and window/dock chrome.
    UiTheme,
    /// Expose virtual media sources and store encrypted auth secrets.
    MediaSource,
    /// Store encrypted credentials in the host OS keychain without exposing a media source.
    Secrets,
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
            Permission::UiOverlay => "视口叠层",
            Permission::UiToolbar => "工具栏扩展",
            Permission::UiModal => "交互弹窗",
            Permission::UiTheme => "外观与布局",
            Permission::MediaSource => "媒体数据源",
            Permission::Secrets => "安全凭据",
            Permission::Notifications => "提示消息",
        }
    }
}

pub fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Command contributed to the host Command Palette (`⌘P` / `Ctrl+P`) and Keymap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub shortcut_hint: Option<String>,
}

/// Declares that a `MediaSource` plugin can dynamically create user-configured
/// connection instances (e.g. multiple WebDAV servers, S3 buckets, or NAS shares).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProviderSpec {
    pub label: String,
    pub add_command_id: String,
}

/// Static plugin manifest (`plugin.json`).
/// Pure declarative UI theme plugins may omit `entry` and specify `theme` / `chrome` directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    #[serde(default)]
    pub sources: Vec<SourceDescriptor>,
    #[serde(default)]
    pub source_provider: Option<SourceProviderSpec>,
    #[serde(default)]
    pub theme: Option<UiThemeSpec>,
    #[serde(default)]
    pub chrome: Option<ChromeLayoutSpec>,
}

fn default_protocol() -> u32 {
    PROTOCOL_VERSION
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: String::new(),
            description: String::new(),
            protocol: PROTOCOL_VERSION,
            entry: None,
            args: Vec::new(),
            permissions: Vec::new(),
            commands: Vec::new(),
            sources: Vec::new(),
            source_provider: None,
            theme: None,
            chrome: None,
        }
    }
}

impl PluginManifest {
    pub fn has_permission(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
            || (perm == Permission::Secrets && self.permissions.contains(&Permission::MediaSource))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_valid_id(&self.id) {
            return Err("invalid plugin id");
        }
        if self.name.trim().is_empty() {
            return Err("empty plugin name");
        }
        if self.protocol != PROTOCOL_VERSION {
            return Err("unsupported protocol version");
        }
        for src in &self.sources {
            if !is_valid_id(&src.id) || src.name.trim().is_empty() {
                return Err("invalid source descriptor");
            }
        }
        if !self.sources.is_empty() && !self.has_permission(Permission::MediaSource) {
            return Err("sources declared without media_source permission");
        }
        if let Some(spec) = &self.source_provider {
            if !self.has_permission(Permission::MediaSource) {
                return Err("source_provider declared without media_source permission");
            }
            if spec.label.trim().is_empty() || !is_valid_id(&spec.add_command_id) {
                return Err("invalid source_provider spec");
            }
        }
        if (self.theme.is_some() || self.chrome.is_some())
            && !self.has_permission(Permission::UiTheme)
        {
            return Err("theme/chrome declared without ui_theme permission");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_enforces_protocol_permissions_source_provider_and_paths() {
        let mut m = PluginManifest {
            id: "acme.stats".into(),
            name: "Stats".into(),
            protocol: PROTOCOL_VERSION,
            entry: Some("main.py".into()),
            permissions: vec![Permission::ReadState],
            ..Default::default()
        };
        assert!(m.validate().is_ok());

        m.protocol = 2;
        assert!(m.validate().is_err(), "only canonical protocol 1 is valid");
        m.protocol = PROTOCOL_VERSION;

        m.sources.push(SourceDescriptor {
            id: "drive".into(),
            name: "Cloud Drive".into(),
            ..Default::default()
        });
        assert!(m.validate().is_err(), "sources require media_source");
        m.sources.clear();

        m.source_provider = Some(SourceProviderSpec {
            label: "WebDAV Server".into(),
            add_command_id: "add_server".into(),
        });
        assert!(m.validate().is_err(), "requires media_source permission");

        m.permissions.push(Permission::MediaSource);
        assert!(m.validate().is_ok());
        assert!(m.has_permission(Permission::Secrets));

        m.source_provider = Some(SourceProviderSpec {
            label: "   ".into(),
            add_command_id: "add_server".into(),
        });
        assert!(m.validate().is_err(), "empty provider label rejected");

        m.source_provider = Some(SourceProviderSpec {
            label: "WebDAV".into(),
            add_command_id: "bad/id".into(),
        });
        assert!(m.validate().is_err(), "invalid add_command_id rejected");
        m.source_provider = None;

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
    }
}
