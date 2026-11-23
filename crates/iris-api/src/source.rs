use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Virtual media source declared by a plugin (cloud drive, NAS protocol, video feed).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub supports_search: bool,
    #[serde(default)]
    pub requires_auth: bool,
    /// Optional plugin command ID invoked when clicking "登录 / 配置" on this source.
    #[serde(default)]
    pub auth_command_id: Option<String>,
    #[serde(default)]
    pub configurable: bool,
    #[serde(default)]
    pub removable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceManageAction {
    Configure,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Photo,
    Audio,
}

/// Read-only playback state snapshot delivered to plugins with [`crate::Permission::ReadState`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    /// Canonical media URI (`file://...`, `webdav://...`, `plugin://...`, or `https://...`).
    pub uri: Option<String>,
    pub title: Option<String>,
    pub kind: Option<MediaKind>,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub favorite: bool,
    pub chapter: i64,
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub buffering: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceEntryKind {
    Folder,
    Video,
    Photo,
    Audio,
    Live,
}

/// Quality / CDN route variant for a playable stream (e.g. 4K, 1080P, Backup).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamVariant {
    pub id: String,
    pub label: String,
}

/// External subtitle stream attached to a resolved video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleAttachment {
    pub label: String,
    #[serde(default)]
    pub lang: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// One item in a virtual directory listing or remote search result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceEntry {
    pub id: String,
    pub name: String,
    pub kind: SourceEntryKind,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified_secs: Option<u64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub variants: Vec<StreamVariant>,
}

/// Directory listing or search page returned by a `MediaSource` plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourcePage {
    pub source_id: String,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub entries: Vec<SourceEntry>,
    #[serde(default)]
    pub next_page: Option<String>,
    #[serde(default = "default_true")]
    pub authenticated: bool,
    #[serde(default)]
    pub auth_hint: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Playable or viewable remote stream descriptor. Validated by the host and
/// passed safely to the media engine without exposing raw engine options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamSpec {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub entry_id: Option<String>,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<SourceEntryKind>,
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub variants: Vec<StreamVariant>,
    #[serde(default)]
    pub active_variant: Option<String>,
    #[serde(default)]
    pub subtitles: Vec<SubtitleAttachment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_descriptor_and_manage_action_serde_defaults() {
        let legacy_json = r#"{"id":"nas","name":"Home NAS"}"#;
        let desc: SourceDescriptor = serde_json::from_str(legacy_json).unwrap();
        assert!(!desc.configurable);
        assert!(!desc.removable);
        assert!(!desc.supports_search);

        let full = SourceDescriptor {
            id: "webdav.office".into(),
            name: "Office WebDAV".into(),
            subtitle: Some("https://dav.example.com".into()),
            supports_search: false,
            requires_auth: true,
            auth_command_id: Some("edit_conn".into()),
            configurable: true,
            removable: true,
        };
        let encoded = serde_json::to_string(&full).unwrap();
        let decoded: SourceDescriptor = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, full);

        assert_eq!(
            serde_json::to_string(&SourceManageAction::Configure).unwrap(),
            r#""configure""#
        );
        assert_eq!(
            serde_json::to_string(&SourceManageAction::Remove).unwrap(),
            r#""remove""#
        );
    }
}
