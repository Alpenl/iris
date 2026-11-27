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

pub use hub::{Binding, Notice, OriginSummary, PluginInfo, PluginStatus, ReviewSummary};
pub use overlay::{OverlayFrame, OverlayGlyphLease, PaintCue};
pub use product::{PluginHost, plugins_dir};
pub use requests::{AcceptedPage, BrowseTarget, RequestTarget, RequestTicket};
pub use runtime::BackgroundResult;
pub use selection::{HostAnnotationJob, HostAnnotationLease, annotation_error_code};
pub use ui::{
    ActionKind, DirtySlots, EnhancementOutcome, EnhancementRequest, Event, OverlayStep,
    POLL_ACTION_LIMIT, POLL_EFFECT_LIMIT, POLL_NOTICE_LIMIT, PollReport, REMOTE_THUMB_LEASE_BYTES,
    REMOTE_THUMB_SCRATCH_BYTES, ReadyAnnotations, ReadyAttachment, ReadyEffect, ReadyMonitorLut,
    ReadyOverlayUpdate, ReadyStreamUpdate, RetainedThumbnail, RevokedEnhancements,
    ThumbnailReceiver, TimelineLease, WindowAction, WindowSession, snapshot_from_input,
};

/// Maps a [`iris_core::bounded_json::BoundedJsonError`] to a protocol-2 [`iris_api::v2::ErrorCode`].
pub(crate) const fn json_error(
    err: iris_core::bounded_json::BoundedJsonError,
) -> iris_api::v2::ErrorCode {
    match err {
        iris_core::bounded_json::BoundedJsonError::QuotaExceeded => {
            iris_api::v2::ErrorCode::QuotaExceeded
        }
        iris_core::bounded_json::BoundedJsonError::Invalid => {
            iris_api::v2::ErrorCode::InvalidArgument
        }
    }
}

/// Converts a protocol-2 plugin `SourceEntry` into a core [`iris_core::catalog::CatalogItem`].
pub fn catalog_item_from_source_entry(
    plugin_id: &str,
    source_id: &str,
    entry: &iris_api::v2::source::SourceEntry,
) -> iris_core::catalog::CatalogItem {
    use iris_api::SourceEntryKind;
    use iris_core::catalog::{CatalogItem, CatalogItemKind, CatalogVariant};
    use iris_core::library::Kind;
    use iris_core::media_uri::MediaUri;

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
    use iris_api::{SourceEntryKind, StreamVariant, v2::WireU64, v2::source::SourceEntry};
    use iris_core::catalog::CatalogItemKind;
    use iris_core::library::Kind;
    use iris_core::media_uri::MediaUri;

    #[test]
    fn catalog_item_maps_v2_source_entry_fields() {
        let entry = SourceEntry {
            id: "ep1".into(),
            name: "Episode 1".into(),
            kind: SourceEntryKind::Video,
            size: Some(WireU64(4096)),
            modified_secs: Some(WireU64(1_700_000_000)),
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
