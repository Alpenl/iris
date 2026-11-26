use super::{
    Id,
    stream::{RemoteResource, StreamSpec},
};
use crate::{MediaKind, SelectOption, SourceEntryKind, StreamVariant};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDescriptor {
    pub id: String,
    pub name: String,
    pub subtitle: Option<String>,
    /// A host icon name, never a URL or filesystem path.
    pub icon: Option<String>,
    /// Optional short host-rendered badge tag (e.g. `"WebDAV"`, `"云源"`), at most 24 bytes.
    #[serde(default)]
    pub badge: Option<String>,
    #[serde(default)]
    pub supports_search: bool,
    #[serde(default)]
    pub requires_auth: bool,
    pub auth_command_id: Option<String>,
    #[serde(default)]
    pub configurable: bool,
    #[serde(default)]
    pub removable: bool,
}
impl SourceDescriptor {
    pub fn validate_all(sources: &[Self]) -> Result<(), &'static str> {
        if sources.len() > 256 {
            return Err("too many sources");
        }
        let mut ids = std::collections::BTreeSet::new();
        for source in sources {
            if !crate::is_valid_id(&source.id)
                || source.name.trim().is_empty()
                || source.name.len() > 256
                || source.name.chars().any(char::is_control)
                || !ids.insert(&source.id)
                || source
                    .subtitle
                    .as_ref()
                    .is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
                || source.icon.as_ref().is_some_and(|i| !crate::is_valid_id(i))
                || source.badge.as_ref().is_some_and(|b| {
                    b.trim().is_empty() || b.len() > 24 || b.chars().any(char::is_control)
                })
                || source
                    .auth_command_id
                    .as_ref()
                    .is_some_and(|id| !crate::is_valid_id(id))
            {
                return Err("invalid source descriptor");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEntry {
    pub id: String,
    pub name: String,
    pub kind: SourceEntryKind,
    pub size: Option<Id>,
    pub modified_secs: Option<Id>,
    pub duration: Option<f64>,
    pub subtitle: Option<String>,
    #[serde(default)]
    pub variants: Vec<StreamVariant>,
    pub thumbnail: Option<RemoteResource>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFilter {
    pub id: String,
    pub label: String,
    pub options: Vec<SelectOption>,
    pub selected: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePage {
    pub source_id: String,
    pub folder_id: Option<String>,
    pub parent_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub entries: Vec<SourceEntry>,
    pub next_page: Option<String>,
    #[serde(default = "default_true")]
    pub authenticated: bool,
    pub auth_hint: Option<String>,
    #[serde(default)]
    pub filters: Vec<SourceFilter>,
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackSnapshot {
    pub media: Option<Id>,
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
    pub chapter: Option<u32>,
    pub is_live: bool,
    pub buffering: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMedia {
    pub stream: StreamSpec,
    #[serde(default)]
    pub playlist: Vec<PlaylistEntry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistEntry {
    pub source_id: String,
    pub entry_id: String,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::SourceDescriptor;

    fn valid_source() -> SourceDescriptor {
        SourceDescriptor {
            id: "nas.main".into(),
            name: "Home NAS".into(),
            subtitle: Some("https://nas.local".into()),
            icon: Some("folder".into()),
            badge: Some("WebDAV".into()),
            supports_search: true,
            requires_auth: true,
            auth_command_id: Some("login".into()),
            configurable: true,
            removable: true,
        }
    }

    #[test]
    fn source_descriptor_validate_all_checks_subtitle_and_auth_command_id() {
        assert!(SourceDescriptor::validate_all(&[valid_source()]).is_ok());

        let mut bad_sub_len = valid_source();
        bad_sub_len.subtitle = Some("a".repeat(257));
        assert!(SourceDescriptor::validate_all(&[bad_sub_len]).is_err());

        let mut bad_sub_ctrl = valid_source();
        bad_sub_ctrl.subtitle = Some("line1\nline2".into());
        assert!(SourceDescriptor::validate_all(&[bad_sub_ctrl]).is_err());

        let mut bad_auth = valid_source();
        bad_auth.auth_command_id = Some("../escape".into());
        assert!(SourceDescriptor::validate_all(&[bad_auth]).is_err());

        let mut bad_name_ctrl = valid_source();
        bad_name_ctrl.name = "NAS\0".into();
        assert!(SourceDescriptor::validate_all(&[bad_name_ctrl]).is_err());

        let mut none_badge = valid_source();
        none_badge.badge = None;
        let encoded = serde_json::to_value(&none_badge).unwrap();
        assert_eq!(encoded.get("badge"), Some(&serde_json::Value::Null));
        let explicit_null: SourceDescriptor = serde_json::from_str(
            r#"{"id":"nas.main","name":"Home NAS","subtitle":null,"icon":null,"badge":null,"auth_command_id":null}"#,
        )
        .unwrap();
        assert_eq!(explicit_null.badge, None);
    }
}
