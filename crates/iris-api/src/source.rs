use serde::{Deserialize, Serialize};

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
#[serde(deny_unknown_fields)]
pub struct StreamVariant {
    pub id: String,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::{MediaKind, SourceEntryKind, SourceManageAction, StreamVariant};

    #[test]
    fn shared_source_value_types_roundtrip() {
        assert_eq!(
            serde_json::to_string(&SourceManageAction::Configure).unwrap(),
            r#""configure""#
        );
        assert_eq!(
            serde_json::to_string(&SourceManageAction::Remove).unwrap(),
            r#""remove""#
        );
        assert_eq!(
            serde_json::to_string(&MediaKind::Video).unwrap(),
            r#""video""#
        );
        assert_eq!(
            serde_json::to_string(&SourceEntryKind::Live).unwrap(),
            r#""live""#
        );
        let variant = StreamVariant {
            id: "1080p".into(),
            label: "1080P".into(),
        };
        let encoded = serde_json::to_string(&variant).unwrap();
        let decoded: StreamVariant = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, variant);
        assert!(
            serde_json::from_str::<StreamVariant>(r#"{"id":"1080p","label":"1080P","extra":1}"#)
                .is_err()
        );
    }
}
