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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
                || !ids.insert(&source.id)
                || source
                    .icon
                    .as_ref()
                    .is_some_and(|i| !crate::is_valid_id(i) || i.len() > 64)
                || source.badge.as_ref().is_some_and(|b| {
                    b.trim().is_empty() || b.len() > 24 || b.chars().any(char::is_control)
                })
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
