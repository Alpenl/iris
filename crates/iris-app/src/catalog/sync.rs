//! Catalog row rebuilding, filtering, sorting, monotonic local-refresh & favorite
//! scan fencing, filmstrip window prioritization, and header/counter projections.

use std::path::{Path, PathBuf};

use iris_core::catalog::{
    CatalogItem, CatalogItemKind, LocalDirectory, MediaFilter, filter_catalog_items,
    sort_catalog_items_with_order,
};
use iris_core::library::{self, FolderEntry, Kind, ScanResult, SortBy, SortOrder};
use iris_core::media_uri::MediaUri;
use iris_core::preset::SidebarMode;
use iris_plugin_host::catalog_item_from_source_entry;

use super::{CatalogController, DirRow};
use crate::plugins::{ActiveSource, PluginController, PluginPlayer};

/// Background favorite-URI metadata resolution job fenced by monotonic `generation`.
#[derive(Clone, Debug)]
pub struct FavoriteScanJob {
    generation: u64,
    uris: Vec<(MediaUri, Option<String>)>,
}

/// Completed favorite scan output paired with the originating `generation` token.
#[derive(Clone, Debug)]
pub struct CompletedFavoriteScan {
    pub generation: u64,
    pub items: Vec<CatalogItem>,
}

impl FavoriteScanJob {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn run(self) -> CompletedFavoriteScan {
        let items = self.uris.into_iter().map(fav_uri_to_catalog_item).collect();
        CompletedFavoriteScan {
            generation: self.generation,
            items,
        }
    }
}

/// Monotonic ticket for an asynchronous local/SMB folder refresh (`scan_local_or_share`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRefreshTicket {
    pub generation: u64,
    pub folder: PathBuf,
}

impl LocalRefreshTicket {
    pub fn run_scan(&self) -> ScanResult {
        library::scan_local_or_share(&self.folder)
    }
}

/// Maps a persisted favorite `(MediaUri, optional recent_title)` into a [`CatalogItem`].
pub fn fav_uri_to_catalog_item((uri, known_title): (MediaUri, Option<String>)) -> CatalogItem {
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

impl CatalogController {
    /// Convenience wrapper rebuilding rows from a [`PluginPlayer`] reference.
    pub fn rebuild_rows_for_player<P: PluginPlayer>(
        &mut self,
        plugins: &PluginController,
        player: &P,
    ) {
        let current_uri = player.current_media_uri();
        let current_local = player.current_local_path();
        self.rebuild_rows(plugins, current_uri.as_ref(), current_local);
    }

    /// Rebuilds sorted source items, media counts, photo filmstrip paths, and filtered `DirRow`s.
    pub fn rebuild_rows(
        &mut self,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) {
        let favorites = self.store.favorites();
        self.counts.favorite = favorites.len();
        if self.favorite_uris != favorites {
            self.favorite_uris = favorites;
            self.favorite_items.clear();
            self.favorite_scan_generation = self.favorite_scan_generation.wrapping_add(1);
            self.favorite_scan_in_flight = None;
        }

        let mut source_items: Vec<_> = match &self.active_source {
            ActiveSource::Local => self.local_dir.to_catalog_items(),
            ActiveSource::Plugin(view) => plugins
                .source_entries(&view.source_id)
                .map(|entry| {
                    catalog_item_from_source_entry(&view.plugin_id, &view.source_id, entry)
                })
                .collect(),
        };
        sort_catalog_items_with_order(&mut source_items, self.sort_by, self.sort_order);

        let (mut videos, mut audio, mut photos) = (0, 0, 0);
        let mut current_index = 0;
        let mut photo_paths = Vec::new();
        for item in &source_items {
            match item.media_kind() {
                Some(Kind::Video) => videos += 1,
                Some(Kind::Audio) => audio += 1,
                Some(Kind::Photo) => {
                    photos += 1;
                    if let Some(path) = item.uri.as_local_path() {
                        if current_local == Some(path) {
                            current_index = photo_paths.len();
                        }
                        photo_paths.push(path.to_path_buf());
                    }
                }
                None => {}
            }
        }
        self.counts.video = videos;
        self.counts.audio = audio;
        self.counts.photo = photos;
        self.filmstrip_index = current_index;
        self.photo_paths = photo_paths;
        self.sorted_source_items = source_items;
        self.filter_rows(current_uri);
    }

    /// Re-filters `rows`, `items`, `playable_count`, and `current_playable_number` using the
    /// active `filter` and `search_query`.
    pub fn filter_rows(&mut self, current_uri: Option<&MediaUri>) {
        let query = self.search_query.as_str();
        let raw_items = if self.filter == MediaFilter::FavoritesOnly {
            &self.favorite_items
        } else {
            &self.sorted_source_items
        };

        let store = &self.store;
        let filtered =
            filter_catalog_items(raw_items, query, self.filter, |u| store.is_favorite(u));
        let mut rows = Vec::with_capacity(filtered.len() + 2);
        if query.trim().is_empty() && self.filter == MediaFilter::All {
            match &self.active_source {
                ActiveSource::Local => {
                    if let Some(parent) = self.local_dir.folder().and_then(Path::parent) {
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

        let (mut playable_count, mut current_playable_number) = (0, None);
        rows.extend(filtered.iter().map(|item| {
            if !item.is_folder() {
                playable_count += 1;
                if current_playable_number.is_none() && current_uri == Some(&item.uri) {
                    current_playable_number = Some(playable_count);
                }
            }
            DirRow::Item(item.clone())
        }));
        if let ActiveSource::Plugin(ps) = &self.active_source
            && let Some(np) = &ps.next_page
            && self.filter != MediaFilter::FavoritesOnly
        {
            rows.push(DirRow::LoadMore {
                plugin_id: ps.plugin_id.clone(),
                source_id: ps.source_id.clone(),
                folder_id: ps.folder_id.clone(),
                next_page: np.clone(),
            });
        }
        self.items = filtered;
        self.playable_count = playable_count;
        self.current_playable_number = current_playable_number;
        self.rows = rows;
    }

    pub fn set_filter(&mut self, filter: MediaFilter, current_uri: Option<&MediaUri>) {
        self.filter = filter;
        self.filter_rows(current_uri);
    }

    pub fn set_search_query(&mut self, query: impl Into<String>, current_uri: Option<&MediaUri>) {
        self.search_query = query.into();
        self.filter_rows(current_uri);
    }

    pub fn clear_search_query(&mut self, current_uri: Option<&MediaUri>) {
        if !self.search_query.is_empty() {
            self.search_query.clear();
        }
        self.filter_rows(current_uri);
    }

    pub fn set_sort(
        &mut self,
        by: SortBy,
        order: SortOrder,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) {
        self.sort_by = by;
        self.sort_order = order;
        self.rebuild_rows(plugins, current_uri, current_local);
    }

    /// Cycles `(SortBy, SortOrder)` through the 6 canonical workspace sort modes and rebuilds rows.
    pub fn cycle_sort(
        &mut self,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) -> (SortBy, SortOrder) {
        let (next_by, next_ord) = match (self.sort_by, self.sort_order) {
            (SortBy::Name, SortOrder::Asc) => (SortBy::Name, SortOrder::Desc),
            (SortBy::Name, SortOrder::Desc) => (SortBy::Modified, SortOrder::Desc),
            (SortBy::Modified, SortOrder::Desc) => (SortBy::Modified, SortOrder::Asc),
            (SortBy::Modified, SortOrder::Asc) => (SortBy::Size, SortOrder::Desc),
            (SortBy::Size, SortOrder::Desc) => (SortBy::Size, SortOrder::Asc),
            (SortBy::Size, SortOrder::Asc) => (SortBy::Name, SortOrder::Asc),
        };
        self.set_sort(next_by, next_ord, plugins, current_uri, current_local);
        (next_by, next_ord)
    }

    /// Installs a newly opened [`LocalDirectory`] and bumps `dir_generation` to invalidate any
    /// in-flight folder refresh scan (caller may follow with [`Self::rebuild_rows`]).
    pub fn install_local_directory(&mut self, dir: LocalDirectory) {
        self.bump_dir_generation();
        self.active_source = ActiveSource::Local;
        self.local_dir = dir;
    }

    /// Installs a newly opened [`LocalDirectory`], bumping `dir_generation` to invalidate any
    /// in-flight folder refresh scan before rebuilding rows.
    pub fn set_local_directory(
        &mut self,
        dir: LocalDirectory,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) {
        self.install_local_directory(dir);
        self.rebuild_rows(plugins, current_uri, current_local);
    }

    /// Issues a monotonic [`LocalRefreshTicket`] if a local folder is currently active.
    pub fn begin_local_directory_refresh(&mut self) -> Option<LocalRefreshTicket> {
        if !self.active_source.is_local() {
            return None;
        }
        let folder = self.local_dir.folder()?.to_path_buf();
        let generation = self.bump_dir_generation();
        self.active_local_refresh = Some((generation, folder.clone()));
        Some(LocalRefreshTicket { generation, folder })
    }

    /// Commits a completed background folder scan if and only if its `ticket` still matches
    /// the current `dir_generation`, active local folder, and non-pending media state.
    /// Late/stale scans are silently discarded (`false`).
    #[allow(clippy::too_many_arguments)]
    pub fn finish_local_directory_refresh(
        &mut self,
        ticket: &LocalRefreshTicket,
        scan: ScanResult,
        media_load_pending: bool,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) -> bool {
        let valid = !media_load_pending
            && self.dir_generation == ticket.generation
            && self.active_source.is_local()
            && self.local_dir.folder() == Some(ticket.folder.as_path())
            && self
                .active_local_refresh
                .as_ref()
                .is_some_and(|(g, f)| *g == ticket.generation && f == &ticket.folder);
        if !valid {
            return false;
        }
        self.active_local_refresh = None;
        self.local_dir.folder = Some(ticket.folder.clone());
        self.local_dir.entries = scan.entries;
        self.local_dir.subfolders = scan.subfolders;
        self.rebuild_rows(plugins, current_uri, current_local);
        true
    }

    /// Prepares a background [`FavoriteScanJob`] when `FavoritesOnly` is active and uncached.
    pub fn begin_favorite_scan(&mut self) -> Option<FavoriteScanJob> {
        if self.filter != MediaFilter::FavoritesOnly
            || self.favorite_scan_in_flight.is_some()
            || !self.favorite_items.is_empty()
            || self.favorite_uris.is_empty()
        {
            return None;
        }
        let generation = self.favorite_scan_generation;
        let uris = self
            .favorite_uris
            .iter()
            .map(|u| (u.clone(), self.store.recent_title(u).map(str::to_owned)))
            .collect();
        self.favorite_scan_in_flight = Some(generation);
        Some(FavoriteScanJob { generation, uris })
    }

    /// Accepts a completed favorite scan if its `generation` matches `favorite_scan_generation`.
    /// Stale results from superseded favorite sets are silently discarded (`false`).
    pub fn finish_favorite_scan(
        &mut self,
        completed: CompletedFavoriteScan,
        current_uri: Option<&MediaUri>,
    ) -> bool {
        if self.favorite_scan_in_flight != Some(completed.generation)
            || completed.generation != self.favorite_scan_generation
        {
            return false;
        }
        self.favorite_scan_in_flight = None;
        self.favorite_items = completed.items;
        if self.filter == MediaFilter::FavoritesOnly {
            self.filter_rows(current_uri);
        }
        true
    }

    /// Returns the row index matching `current_uri` for scroll-to-current navigation.
    pub fn current_row_index(&self, current_uri: Option<&MediaUri>) -> Option<usize> {
        let cur = current_uri?;
        self.rows
            .iter()
            .position(|r| matches!(r, DirRow::Item(it) if &it.uri == cur))
    }

    /// Computes the center-outward prioritized window of up to 13 local photo paths for filmstrip decoding.
    pub fn prioritized_filmstrip_window(
        &self,
        is_photo: bool,
        filmstrip_enabled: bool,
    ) -> Vec<PathBuf> {
        if !is_photo || !filmstrip_enabled || self.photo_paths.len() < 2 {
            return Vec::new();
        }
        let cur_ix = self
            .filmstrip_index
            .min(self.photo_paths.len().saturating_sub(1));
        let mut prioritized = Vec::with_capacity(13);
        prioritized.push(self.photo_paths[cur_ix].clone());
        for d in 1..=6usize {
            if let Some(p) = self.photo_paths.get(cur_ix + d) {
                prioritized.push(p.clone());
            }
            if cur_ix >= d {
                prioritized.push(self.photo_paths[cur_ix - d].clone());
            }
        }
        prioritized
    }

    pub fn sidebar_header_title(&self) -> String {
        match self.sidebar_mode {
            SidebarMode::Files => match &self.active_source {
                ActiveSource::Local => self
                    .local_dir
                    .folder()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "本地目录".into()),
                ActiveSource::Plugin(ps) => ps.title.clone(),
            },
            SidebarMode::Sources => "数据源与书签".into(),
            SidebarMode::Queue => "独立播放队列".into(),
            SidebarMode::Plugins => "插件工作台".into(),
        }
    }

    pub fn titlebar_folder_label(&self, has_remote_session_uri: bool) -> String {
        if !self.active_source.is_local() {
            self.active_source
                .display_badge()
                .unwrap_or_else(|| "远程数据源".into())
        } else {
            self.local_dir
                .folder()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    if has_remote_session_uri {
                        "远程媒体".into()
                    } else {
                        "未打开".into()
                    }
                })
        }
    }

    pub fn photo_counter_strings(
        &self,
        current_uri: Option<&MediaUri>,
        has_local_current: bool,
        has_any_session_uri: bool,
    ) -> (String, String) {
        let standalone_remote = self.current_playable_number.is_none()
            && !has_local_current
            && self.active_source.is_local()
            && self.filter != MediaFilter::FavoritesOnly;
        if standalone_remote {
            if !self.queue.is_empty() {
                let pos = current_uri
                    .and_then(|u| self.queue.position_of(u))
                    .map_or_else(|| "—".to_string(), |i| (i + 1).to_string());
                (pos, self.queue.len().to_string())
            } else if has_any_session_uri {
                ("1".to_string(), "1".to_string())
            } else {
                ("—".to_string(), "0".to_string())
            }
        } else {
            let pos = self
                .current_playable_number
                .map_or_else(|| "—".to_string(), |number| number.to_string());
            (pos, self.playable_count.to_string())
        }
    }
}
