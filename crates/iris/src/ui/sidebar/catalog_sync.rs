//! Catalog row & count synchronization (`rebuild_directory_rows`, `scroll_directory_to_current`,
//! `submit_directory_search`) bridging `Session` / `ActiveSource` / `Store` to `CatalogState`.

use std::path::Path;

use gpui::{Context, ScrollStrategy};
use iris_core::catalog::{
    CatalogItem, CatalogItemKind, MediaFilter, filter_catalog_items, sort_catalog_items_with_order,
};
use iris_core::library::{self, FolderEntry, Kind, ScanResult};
use iris_core::media_uri::MediaUri;

use super::super::IrisWindow;
use super::super::state::ActiveSource;
use super::DirRow;

fn fav_uri_to_catalog_item(uri: MediaUri) -> CatalogItem {
    match &uri {
        MediaUri::Local(p) if p.is_dir() => CatalogItem::from_local_folder(&FolderEntry {
            name: uri.display_name(),
            path: p.clone(),
        }),
        MediaUri::Local(p) => {
            let fallback_kind = CatalogItemKind::Media(library::kind_of(p).unwrap_or(Kind::Video));
            library::entry_from_path(p.clone())
                .map(|e| CatalogItem::from_local_entry(&e))
                .unwrap_or_else(|| CatalogItem {
                    name: uri.display_name(),
                    uri,
                    kind: fallback_kind,
                    subtitle: None,
                    size: 0,
                    modified_secs: 0,
                    duration: None,
                    is_live: false,
                    variants: Vec::new(),
                })
        }
        _ => {
            let name = uri.display_name();
            let kind =
                CatalogItemKind::Media(library::kind_of(Path::new(&name)).unwrap_or(Kind::Video));
            CatalogItem {
                uri,
                name,
                kind,
                subtitle: None,
                size: 0,
                modified_secs: 0,
                duration: None,
                is_live: false,
                variants: Vec::new(),
            }
        }
    }
}

impl IrisWindow {
    pub(in crate::ui) fn submit_directory_search(&mut self, cx: &mut Context<Self>) {
        if let ActiveSource::Plugin(ps) = &self.catalog.active_source
            && ps.supports_search
        {
            let (pid, sid, fid) = (
                ps.plugin_id.clone(),
                ps.source_id.clone(),
                ps.folder_id.clone(),
            );
            let q = (!self.text_inputs.search_input.is_empty())
                .then(|| self.text_inputs.search_input.text().trim().to_string())
                .filter(|s| !s.is_empty());
            self.browse_plugin_source(&pid, &sid, fid, None, q, cx);
        } else {
            self.rebuild_directory_rows();
            cx.notify();
        }
    }

    pub(in crate::ui) fn scroll_directory_to_current(&self) {
        let Some(cur) = self.active().session.current_media_uri() else {
            return;
        };
        if let Some(ix) = self
            .catalog
            .rows
            .iter()
            .position(|r| matches!(r, DirRow::Item(it) if it.uri == cur))
        {
            self.catalog
                .dir_scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    pub(in crate::ui) fn rebuild_directory_rows(&mut self) {
        let favs = self.store.favorites();
        self.catalog.favorite_count = favs.len();
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text()
        } else {
            ""
        };

        let raw_items: Vec<CatalogItem> = if self.catalog.filter == MediaFilter::FavoritesOnly {
            favs.into_iter().map(fav_uri_to_catalog_item).collect()
        } else {
            match &self.catalog.active_source {
                ActiveSource::Local => {
                    let session = &self.active().session;
                    let mut all = CatalogItem::from_scan_result(&ScanResult {
                        entries: session.entries().to_vec(),
                        subfolders: session.subfolders().to_vec(),
                    });
                    sort_catalog_items_with_order(
                        &mut all,
                        self.settings.sort_by,
                        self.settings.sort_order,
                    );
                    self.catalog.video_audio_count = all
                        .iter()
                        .filter(|i| matches!(i.media_kind(), Some(Kind::Video | Kind::Audio)))
                        .count();
                    self.catalog.photo_count = all
                        .iter()
                        .filter(|i| i.media_kind() == Some(Kind::Photo))
                        .count();
                    self.catalog.photo_paths = all
                        .iter()
                        .filter(|i| i.media_kind() == Some(Kind::Photo))
                        .filter_map(|i| i.uri.as_local_path().map(Path::to_path_buf))
                        .collect();
                    all
                }
                ActiveSource::Plugin(ps) => {
                    let mut all: Vec<CatalogItem> = ps
                        .entries
                        .iter()
                        .map(|e| CatalogItem::from_source_entry(&ps.plugin_id, &ps.source_id, e))
                        .collect();
                    sort_catalog_items_with_order(
                        &mut all,
                        self.settings.sort_by,
                        self.settings.sort_order,
                    );
                    self.catalog.video_audio_count = all
                        .iter()
                        .filter(|i| matches!(i.media_kind(), Some(Kind::Video | Kind::Audio)))
                        .count();
                    self.catalog.photo_count = all
                        .iter()
                        .filter(|i| i.media_kind() == Some(Kind::Photo))
                        .count();
                    self.catalog.photo_paths.clear();
                    all
                }
            }
        };

        let filtered = filter_catalog_items(&raw_items, query, self.catalog.filter, |u| {
            self.store.is_favorite(u)
        });
        let mut rows = Vec::with_capacity(filtered.len() + 2);
        if query.trim().is_empty() && self.catalog.filter == MediaFilter::All {
            match &self.catalog.active_source {
                ActiveSource::Local => {
                    if let Some(parent) = self.active().session.folder().and_then(Path::parent) {
                        rows.push(DirRow::Up {
                            parent_local: Some(parent.to_path_buf()),
                            parent_plugin: None,
                        });
                    }
                }
                ActiveSource::Plugin(ps) => {
                    let at_sub = ps.parent_id.is_some()
                        || ps
                            .folder_id
                            .as_deref()
                            .is_some_and(|f| !f.is_empty() && f != "/");
                    if at_sub {
                        rows.push(DirRow::Up {
                            parent_local: None,
                            parent_plugin: Some((
                                ps.plugin_id.clone(),
                                ps.source_id.clone(),
                                ps.parent_id.clone(),
                            )),
                        });
                    }
                }
            }
        }
        rows.extend(filtered.iter().cloned().map(DirRow::Item));
        if let ActiveSource::Plugin(ps) = &self.catalog.active_source
            && let Some(np) = &ps.next_page
            && self.catalog.filter != MediaFilter::FavoritesOnly
        {
            rows.push(DirRow::LoadMore {
                plugin_id: ps.plugin_id.clone(),
                source_id: ps.source_id.clone(),
                folder_id: ps.folder_id.clone(),
                next_page: np.clone(),
            });
        }
        self.catalog.items = filtered;
        self.catalog.rows = rows;
    }
}
