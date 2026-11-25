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
pub mod json;
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
pub mod tools;
pub mod ui;
pub mod url_resolver;
pub mod validation;

/// Converts a protocol-2 plugin `SourceEntry` into a core [`crate::catalog::CatalogItem`].
pub fn catalog_item_from_source_entry(
    plugin_id: &str,
    source_id: &str,
    entry: &iris_api::SourceEntry,
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
        size: entry.size.unwrap_or(0),
        modified_secs: entry.modified_secs.unwrap_or(0),
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
