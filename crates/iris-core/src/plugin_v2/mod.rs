//! Sandboxed protocol-2 runtime, installation, capabilities and presentation.
pub mod actor;
pub mod auth;
pub mod crypto;
pub mod engine;
pub mod enhancement;
pub mod files;
pub mod hub;
mod imports;
pub mod installation;
pub mod instance;
pub mod ledger;
pub mod lut;
pub mod network;
pub mod official;
pub mod overlay;
pub mod package;
pub mod pool;
pub mod product;
pub mod requests;
pub mod runtime;
pub mod selection;
pub mod services;
pub mod storage;
pub mod stream;
pub mod ui;
pub mod url_resolver;
pub mod validation;

/// Maps a [`crate::bounded_json::BoundedJsonError`] to a protocol-2 [`iris_api::v2::ErrorCode`].
pub(crate) const fn json_error(
    err: crate::bounded_json::BoundedJsonError,
) -> iris_api::v2::ErrorCode {
    match err {
        crate::bounded_json::BoundedJsonError::QuotaExceeded => {
            iris_api::v2::ErrorCode::QuotaExceeded
        }
        crate::bounded_json::BoundedJsonError::Invalid => iris_api::v2::ErrorCode::InvalidArgument,
    }
}

/// Converts a protocol-2 plugin `SourceEntry` into a core [`crate::catalog::CatalogItem`].
pub fn catalog_item_from_source_entry(
    plugin_id: &str,
    source_id: &str,
    entry: &iris_api::v2::source::SourceEntry,
) -> crate::catalog::CatalogItem {
    use crate::catalog::{CatalogItem, CatalogItemKind, CatalogVariant};
    use crate::library::Kind;
    use crate::media_uri::MediaUri;
    use iris_api::SourceEntryKind;

    let (kind, is_live) = match entry.kind {
        SourceEntryKind::Folder => (
            CatalogItemKind::Folder {
                folder_id: Some(entry.id.clone()),
            },
            false,
        ),
        SourceEntryKind::Video => (CatalogItemKind::Media(Kind::Video), false),
        SourceEntryKind::Photo => (CatalogItemKind::Media(Kind::Photo), false),
        SourceEntryKind::Audio => (CatalogItemKind::Media(Kind::Audio), false),
        SourceEntryKind::Live => (CatalogItemKind::Media(Kind::Video), true),
    };
    CatalogItem {
        uri: MediaUri::Plugin {
            plugin_id: plugin_id.to_string(),
            source_id: source_id.to_string(),
            entry_id: entry.id.clone(),
        },
        name: entry.name.clone(),
        kind,
        size: entry.size.map_or(0, |v| v.0),
        modified_secs: entry.modified_secs.map_or(0, |v| v.0),
        duration: entry.duration,
        subtitle: entry.subtitle.clone(),
        is_live,
        variants: entry
            .variants
            .iter()
            .map(|v| CatalogVariant {
                id: v.id.clone(),
                label: v.label.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::catalog_item_from_source_entry;
    use crate::{catalog::CatalogItemKind, library::Kind, media_uri::MediaUri};
    use iris_api::{SourceEntryKind, StreamVariant, v2::Id, v2::source::SourceEntry};

    #[test]
    fn catalog_item_maps_v2_source_entry_fields() {
        let entry = SourceEntry {
            id: "ep1".into(),
            name: "Episode 1".into(),
            kind: SourceEntryKind::Video,
            size: Some(Id(4096)),
            modified_secs: Some(Id(1_700_000_000)),
            duration: Some(123.5),
            subtitle: Some("1080p".into()),
            variants: vec![StreamVariant {
                id: "hd".into(),
                label: "HD".into(),
            }],
            thumbnail: None,
        };
        let item = catalog_item_from_source_entry("iris.webdav", "nas", &entry);
        assert_eq!(
            item.uri,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "nas".into(),
                entry_id: "ep1".into(),
            }
        );
        assert_eq!(item.kind, CatalogItemKind::Media(Kind::Video));
        assert_eq!(item.size, 4096);
        assert_eq!(item.modified_secs, 1_700_000_000);
        assert_eq!(item.duration, Some(123.5));
        assert_eq!(item.variants.len(), 1);
    }
}
