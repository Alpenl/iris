//! Catalog row & count synchronization (`rebuild_directory_rows`, `scroll_directory_to_current`,
//! `submit_directory_search`) bridging `Session` / `ActiveSource` / `Store` to `CatalogState`.

use std::path::Path;

use gpui::{Context, ScrollStrategy};
use iris_core::catalog::{
    CatalogItem, CatalogItemKind, MediaFilter, filter_catalog_items, sort_catalog_items_with_order,
};
use iris_core::library::{self, FolderEntry, Kind};
use iris_core::media_uri::MediaUri;

use super::super::IrisWindow;
use super::super::state::ActiveSource;
use super::DirRow;

fn fav_uri_to_catalog_item((uri, known_title): (MediaUri, Option<String>)) -> CatalogItem {
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
            let fallback_name = uri.display_name();
            let name = known_title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| fallback_name.clone());
            let kind = CatalogItemKind::Media(
                library::kind_of(Path::new(&fallback_name))
                    .or_else(|| library::kind_of(Path::new(&name)))
                    .unwrap_or(Kind::Video),
            );
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
            self.filter_directory_rows();
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
        let favorites = self.store.favorites();
        self.catalog.favorite_count = favorites.len();
        if self.catalog.favorite_uris != favorites {
            self.catalog.favorite_uris = favorites;
            self.catalog.favorite_items.clear();
            self.catalog.favorite_scan = None;
        }

        let mut source_items: Vec<_> = match &self.catalog.active_source {
            ActiveSource::Local => self.catalog.local_dir.to_catalog_items(),
            ActiveSource::Plugin(view) => self
                .plugin_source_entries(&view.source_id)
                .map(|entry| {
                    iris_core::plugin_v2::catalog_item_from_source_entry(
                        &view.plugin_id,
                        &view.source_id,
                        entry,
                    )
                })
                .collect(),
        };
        sort_catalog_items_with_order(
            &mut source_items,
            self.settings.sort_by,
            self.settings.sort_order,
        );

        let (mut videos, mut audio, mut photos) = (0, 0, 0);
        let current = self.active().session.current();
        let mut current_index = 0;
        let mut photo_paths = Vec::new();
        for item in &source_items {
            match item.media_kind() {
                Some(Kind::Video) => videos += 1,
                Some(Kind::Audio) => audio += 1,
                Some(Kind::Photo) => {
                    photos += 1;
                    if let Some(path) = item.uri.as_local_path() {
                        if current == Some(path) {
                            current_index = photo_paths.len();
                        }
                        photo_paths.push(path.to_path_buf());
                    }
                }
                None => {}
            }
        }
        self.catalog.video_count = videos;
        self.catalog.audio_count = audio;
        self.catalog.photo_count = photos;
        self.catalog.filmstrip_index = current_index;
        self.catalog.photo_paths = photo_paths;
        self.catalog.sorted_source_items = source_items;
        self.filter_directory_rows();
    }

    pub(in crate::ui) fn scan_visible_favorites(&mut self, cx: &mut Context<Self>) {
        if self.catalog.filter != MediaFilter::FavoritesOnly
            || self.catalog.favorite_scan.is_some()
            || self.catalog.favorite_uris.is_empty()
        {
            return;
        }
        let uris: Vec<(MediaUri, Option<String>)> = self
            .catalog
            .favorite_uris
            .iter()
            .map(|u| (u.clone(), self.store.recent_title(u).map(str::to_owned)))
            .collect();
        let scan = cx
            .background_executor()
            .spawn(async move { uris.into_iter().map(fav_uri_to_catalog_item).collect() });
        self.catalog.favorite_scan = Some(cx.spawn(async move |this, cx| {
            let items = scan.await;
            let _ = this.update(cx, |this, cx| {
                this.catalog.favorite_items = items;
                if this.catalog.filter == MediaFilter::FavoritesOnly {
                    this.filter_directory_rows();
                    cx.notify();
                }
            });
        }));
    }

    pub(in crate::ui) fn filter_directory_rows(&mut self) {
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text()
        } else {
            ""
        };
        let raw_items = if self.catalog.filter == MediaFilter::FavoritesOnly {
            &self.catalog.favorite_items
        } else {
            &self.catalog.sorted_source_items
        };

        let filtered = filter_catalog_items(raw_items, query, self.catalog.filter, |u| {
            self.store.is_favorite(u)
        });
        let mut rows = Vec::with_capacity(filtered.len() + 2);
        if query.trim().is_empty() && self.catalog.filter == MediaFilter::All {
            match &self.catalog.active_source {
                ActiveSource::Local => {
                    if let Some(parent) = self.catalog.local_dir.folder().and_then(Path::parent) {
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
        // Media-open completion also rebuilds these rows, so the counter is
        // refreshed on both catalog changes and changes to the current item.
        let current = self.active().session.current_media_uri();
        let (mut playable_count, mut current_playable_number) = (0, None);
        rows.extend(filtered.iter().map(|item| {
            if !item.is_folder() {
                playable_count += 1;
                if current_playable_number.is_none() && current.as_ref() == Some(&item.uri) {
                    current_playable_number = Some(playable_count);
                }
            }
            DirRow::Item(item.clone())
        }));
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
        self.catalog.playable_count = playable_count;
        self.catalog.current_playable_number = current_playable_number;
        self.catalog.rows = rows;
    }
}
