use super::catalog_item_from_source_entry;
use iris_api::{SourceEntryKind, StreamVariant, v2::WireU64, v2::source::SourceEntry};
use iris_core::catalog::CatalogItemKind;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;

mod fuzz;
mod guest_contracts;
mod wat_abi;

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
