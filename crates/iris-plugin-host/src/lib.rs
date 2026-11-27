//! Sandboxed protocol-2 runtime, installation, capabilities and presentation.
#![deny(unreachable_pub, unnameable_types, private_interfaces, private_bounds)]

mod actor;
mod auth;
mod crypto;
mod engine;
mod enhancement;
mod files;
mod hub;
mod imports;
mod installation;
mod instance;
mod ledger;
mod lut;
mod network;
mod official;
mod overlay;
mod package;
mod pool;
mod product;
mod requests;
mod runtime;
mod selection;
mod services;
mod storage;
mod stream;
#[cfg(feature = "test-support")]
mod test_support;
mod ui;
mod url_resolver;
mod validation;

pub use hub::{
    Binding, Commands, Notice, OriginSummary, PluginInfo, PluginStatus, ReviewSummary,
    SourceSnapshot,
};
pub use installation::{ArchivedIdentity, Installed};
pub use network::consent::SavedOrigin;
pub use official::smoke::check as check_embedded_runtime;
pub use overlay::{OverlayFrame, OverlayGlyphLease, PaintCue};
pub use product::{PluginHost, plugins_dir};
pub use requests::{AcceptedPage, BrowseTarget, RequestTarget, RequestTicket};
pub use runtime::BackgroundResult;
pub use selection::{HostAnnotationJob, HostAnnotationLease, annotation_error_code};
pub use stream::prepare::{AttachmentView, Playback};
#[cfg(feature = "test-support")]
pub use test_support::{StreamReadMemoryProbe, open_host_with_bundled_package};
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
mod tests;
