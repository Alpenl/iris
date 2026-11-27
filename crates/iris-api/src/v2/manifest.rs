use super::source::SourceDescriptor;
use crate::{ChromeLayoutSpec, CommandSpec, SourceProviderSpec, UiThemeSpec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadState,
    ControlPlayback,
    MediaSource,
    UrlResolver,
    MediaEnhance,
    Annotations,
    Secrets,
    Notifications,
    UiSidebar,
    UiStatus,
    UiOverlay,
    UiToolbar,
    UiModal,
    UiTheme,
    NetHttp,
    NetWs,
    NetTcp,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDeclaration {
    #[serde(default)]
    pub net_public: bool,
    #[serde(default)]
    pub net_hosts: Vec<HostRule>,
    #[serde(default)]
    pub public_ports: Vec<u16>,
    #[serde(default)]
    pub tcp_targets: Vec<HostRule>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRule {
    pub host: String,
    pub schemes: Vec<String>,
    pub ports: Vec<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol: u32,
    pub abi: u32,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    pub entry: Option<String>,
    #[serde(default)]
    pub permissions: Vec<Capability>,
    #[serde(default)]
    pub network: NetworkDeclaration,
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
    #[serde(default)]
    pub url_patterns: Vec<String>,
    /// Plugin-specific secret/cookie/parameter names redacted by the host logger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_keys: Vec<String>,
}

impl Manifest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.protocol != crate::PROTOCOL_VERSION || self.abi != super::ABI_VERSION {
            return Err("unsupported protocol or ABI");
        }
        if !crate::is_valid_id(&self.id)
            || self.name.trim().is_empty()
            || self.name.len() > 256
            || self.version.is_empty()
            || self.version.len() > 64
        {
            return Err("invalid plugin identity");
        }
        if self
            .entry
            .as_deref()
            .is_some_and(|entry| entry != "plugin.wasm")
        {
            return Err("entry must be plugin.wasm");
        }
        if self.entry.is_none() && self.theme.is_none() && self.chrome.is_none() {
            return Err("missing module or declarative theme");
        }
        let unique: std::collections::BTreeSet<_> = self.permissions.iter().collect();
        if unique.len() != self.permissions.len() {
            return Err("duplicate permission");
        }
        if self.commands.len() > 256
            || self.url_patterns.len() > 64
            || self.sensitive_keys.len() > 64
            || self.network.net_hosts.len() > 128
            || self.network.tcp_targets.len() > 32
            || self.network.public_ports.len() > 32
        {
            return Err("too many manifest contributions");
        }
        let mut sensitive = std::collections::BTreeSet::new();
        for key in &self.sensitive_keys {
            let trimmed = key.trim_matches(['_', '-']);
            if trimmed.len() < 3
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || !sensitive.insert(key.to_ascii_lowercase())
            {
                return Err("invalid sensitive key");
            }
        }
        SourceDescriptor::validate_all(&self.sources)?;
        let mut commands = std::collections::BTreeSet::new();
        for cmd in &self.commands {
            if !crate::is_valid_id(&cmd.id)
                || cmd.title.trim().is_empty()
                || cmd.title.len() > 256
                || cmd.subtitle.as_ref().is_some_and(|v| v.len() > 512)
                || cmd.shortcut_hint.as_ref().is_some_and(|v| v.len() > 64)
                || !commands.insert(&cmd.id)
            {
                return Err("invalid command");
            }
        }
        if self.commands.iter().filter(|v| v.context_menu).count() > 16 {
            return Err("too many context menu commands");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    fn sample_manifest() -> Manifest {
        serde_json::from_str(
            r#"{
                "id": "community.demo",
                "name": "Demo",
                "version": "0.0.2",
                "protocol": 2,
                "abi": 2,
                "entry": "plugin.wasm"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn manifest_validates_sensitive_keys_format_and_duplicates() {
        let mut m = sample_manifest();
        assert!(m.validate().is_ok());

        m.sensitive_keys = vec!["sessdata".into(), "bili_jct".into(), "x-custom-auth".into()];
        assert!(m.validate().is_ok());

        // Case-insensitive duplicates rejected.
        m.sensitive_keys = vec!["sessdata".into(), "SESSDATA".into()];
        assert!(m.validate().is_err());

        // Overly short keys (< 3 chars after trimming) or non-key characters rejected.
        for bad in ["a", "id", "__a__", "bad key", "bad=key"] {
            m.sensitive_keys = vec![bad.into()];
            assert!(m.validate().is_err(), "{bad}");
        }
    }
}
