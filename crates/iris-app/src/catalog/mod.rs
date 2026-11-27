//! Headless [`CatalogController`] vertical slice: single authoritative owner of
//! directory catalog browsing, filter/sort/search state, monotonic async request
//! generations, cross-source [`PlayQueue`], pinned [`LocationStore`] bookmarks,
//! and [`Store`] persistence (`last_save`, flush intents, and completion receipts).
//!
//! Intentionally free of `gpui` types (`RenderImage`, `Task`, `UniformListScrollHandle`),
//! window visibility flags, and `DirtyRegions` UI invalidation policies.

mod navigation;
mod persistence;
mod sync;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use iris_base::net::http_client::HttpCancel;
use iris_core::bookmarks::{LocationStore, PinnedBookmark};
use iris_core::catalog::{CatalogItem, LocalDirectory, MediaFilter, PlayQueue};
use iris_core::library::{ScanResult, SortBy, SortOrder};
use iris_core::media_uri::MediaUri;
use iris_core::preset::SidebarMode;
use iris_core::store::Store;

use crate::plugins::{ActiveSource, PluginController};

pub use navigation::{
    BookmarkAction, DirectStreamTarget, OpenBatchPlan, OpenMediaBatchAction, StepAction,
    can_step_queue, parse_direct_stream_input, plan_open_paths, resolve_queue_paths,
};
pub use persistence::{SAVE_EVERY, StoreFlushIntent, StoreFlushReceipt};
pub use sync::{
    CompletedFavoriteScan, FavoriteScanJob, LocalRefreshTicket, fav_uri_to_catalog_item,
};

use persistence::StoreFlushState;

/// Pure domain row model driving the unified `Files` virtual list across local
/// folders, `MediaSource` plugins, and favorites.
#[derive(Clone, Debug, PartialEq)]
pub enum DirRow {
    Up {
        parent_local: Option<PathBuf>,
        parent_plugin: Option<(String, String, Option<String>)>,
    },
    Item(CatalogItem),
    LoadMore {
        plugin_id: String,
        source_id: String,
        folder_id: Option<String>,
        next_page: String,
    },
}

/// Per-category item counts across the active catalog source and favorites store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogCounts {
    pub video: usize,
    pub audio: usize,
    pub photo: usize,
    pub favorite: usize,
}

impl CatalogCounts {
    pub const fn total_media(self) -> usize {
        self.video + self.audio + self.photo
    }
}

/// RAII guard cancelling an in-flight remote stream probe on replacement or drop.
struct RemoteLoadCancel(HttpCancel);

impl Drop for RemoteLoadCancel {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Single authoritative owner of directory browsing, playback queue, bookmarks,
/// and media-record persistence across an application window.
pub struct CatalogController {
    sidebar_mode: SidebarMode,
    active_source: ActiveSource,
    local_dir: LocalDirectory,
    filter: MediaFilter,
    sort_by: SortBy,
    sort_order: SortOrder,
    search_query: String,
    rows: Vec<DirRow>,
    items: Vec<CatalogItem>,
    playable_count: usize,
    current_playable_number: Option<usize>,
    sorted_source_items: Vec<CatalogItem>,
    favorite_uris: Vec<MediaUri>,
    favorite_items: Vec<CatalogItem>,
    photo_paths: Vec<PathBuf>,
    filmstrip_index: usize,
    counts: CatalogCounts,
    dir_generation: u64,
    active_local_refresh: Option<(u64, PathBuf)>,
    favorite_scan_generation: u64,
    favorite_scan_in_flight: Option<u64>,
    remote_load: Option<RemoteLoadCancel>,
    remote_work_slot: Arc<futures::lock::Mutex<()>>,
    queue: PlayQueue,
    locations: LocationStore,
    store: Store,
    last_save: Instant,
    flush_state: StoreFlushState,
}

impl CatalogController {
    /// Loads [`LocationStore`] and [`Store`] from the default application data directory.
    pub fn load(
        sidebar_mode: SidebarMode,
        sort_by: SortBy,
        sort_order: SortOrder,
        now: Instant,
    ) -> Self {
        Self::with_stores(
            LocationStore::load(),
            Store::load(),
            sidebar_mode,
            sort_by,
            sort_order,
            now,
        )
    }

    /// Constructs a [`CatalogController`] from explicit [`LocationStore`] and [`Store`] instances
    /// (used by isolated headless tests without touching user data directories).
    pub fn with_stores(
        locations: LocationStore,
        store: Store,
        sidebar_mode: SidebarMode,
        sort_by: SortBy,
        sort_order: SortOrder,
        now: Instant,
    ) -> Self {
        let favorites = store.favorites();
        let favorite_count = favorites.len();
        Self {
            sidebar_mode,
            active_source: ActiveSource::Local,
            local_dir: LocalDirectory::default(),
            filter: MediaFilter::All,
            sort_by,
            sort_order,
            search_query: String::new(),
            rows: Vec::new(),
            items: Vec::new(),
            playable_count: 0,
            current_playable_number: None,
            sorted_source_items: Vec::new(),
            favorite_uris: favorites,
            favorite_items: Vec::new(),
            photo_paths: Vec::new(),
            filmstrip_index: 0,
            counts: CatalogCounts {
                favorite: favorite_count,
                ..CatalogCounts::default()
            },
            dir_generation: 0,
            active_local_refresh: None,
            favorite_scan_generation: 0,
            favorite_scan_in_flight: None,
            remote_load: None,
            remote_work_slot: Arc::new(futures::lock::Mutex::new(())),
            queue: PlayQueue::new(),
            locations,
            store,
            last_save: now,
            flush_state: StoreFlushState::default(),
        }
    }

    pub fn sidebar_mode(&self) -> SidebarMode {
        self.sidebar_mode
    }

    pub fn set_sidebar_mode(&mut self, mode: SidebarMode) {
        self.sidebar_mode = mode;
    }

    pub fn cycle_sidebar_mode(&mut self) -> SidebarMode {
        let next = match self.sidebar_mode {
            SidebarMode::Files => SidebarMode::Sources,
            SidebarMode::Sources => SidebarMode::Queue,
            SidebarMode::Queue => SidebarMode::Plugins,
            SidebarMode::Plugins => SidebarMode::Files,
        };
        self.sidebar_mode = next;
        next
    }

    pub fn active_source(&self) -> &ActiveSource {
        &self.active_source
    }

    pub fn local_dir(&self) -> &LocalDirectory {
        &self.local_dir
    }

    pub fn filter(&self) -> MediaFilter {
        self.filter
    }

    pub fn sort_by(&self) -> SortBy {
        self.sort_by
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    pub fn rows(&self) -> &[DirRow] {
        &self.rows
    }

    pub fn items(&self) -> &[CatalogItem] {
        &self.items
    }

    pub fn playable_count(&self) -> usize {
        self.playable_count
    }

    pub fn current_playable_number(&self) -> Option<usize> {
        self.current_playable_number
    }

    pub fn favorite_items(&self) -> &[CatalogItem] {
        &self.favorite_items
    }

    pub fn photo_paths(&self) -> &[PathBuf] {
        &self.photo_paths
    }

    pub fn filmstrip_index(&self) -> usize {
        self.filmstrip_index
    }

    pub fn counts(&self) -> CatalogCounts {
        self.counts
    }

    pub fn dir_generation(&self) -> u64 {
        self.dir_generation
    }

    pub fn favorite_scan_generation(&self) -> u64 {
        self.favorite_scan_generation
    }

    pub fn is_favorite_scan_in_flight(&self) -> bool {
        self.favorite_scan_in_flight.is_some()
    }

    pub fn queue(&self) -> &PlayQueue {
        &self.queue
    }

    pub fn locations(&self) -> &LocationStore {
        &self.locations
    }

    pub fn bookmarks(&self) -> &[PinnedBookmark] {
        &self.locations.bookmarks
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn last_save(&self) -> Instant {
        self.last_save
    }

    pub fn has_remote_load(&self) -> bool {
        self.remote_load.is_some()
    }

    /// Starts a tracked remote stream preparation, cancelling any superseded load and
    /// returning its cancellation token plus the serialized header-probe mutex.
    pub fn begin_remote_load(&mut self) -> (HttpCancel, Arc<futures::lock::Mutex<()>>) {
        let cancel = HttpCancel::default();
        self.remote_load = Some(RemoteLoadCancel(cancel.clone()));
        (cancel, Arc::clone(&self.remote_work_slot))
    }

    /// Cancels any active remote stream preparation and returns `true` if one was active.
    pub fn cancel_remote_load(&mut self) -> bool {
        self.remote_load.take().is_some()
    }

    pub fn remote_work_slot(&self) -> &Arc<futures::lock::Mutex<()>> {
        &self.remote_work_slot
    }

    /// Splits mutable borrows of [`Store`] and [`ActiveSource`] for [`crate::PluginContext`],
    /// caller-paired with [`Self::reconcile_plugin_store_edits`] afterwards.
    pub fn plugin_context_parts_mut(&mut self) -> (&mut Store, &mut ActiveSource) {
        (&mut self.store, &mut self.active_source)
    }

    /// Executes `op` against [`PluginController`] with borrowed `store` and `active_source`,
    /// automatically reconciling any `Store` dirty-bit or `ActiveSource` generation transitions.
    pub fn with_plugin_parts<R>(
        &mut self,
        op: impl FnOnce(&mut Store, &mut ActiveSource) -> R,
    ) -> R {
        let was_plugin = !self.active_source.is_local();
        let out = op(&mut self.store, &mut self.active_source);
        if was_plugin && self.active_source.is_local() {
            self.bump_dir_generation();
        }
        self.reconcile_plugin_store_edits();
        out
    }

    pub(crate) fn bump_dir_generation(&mut self) -> u64 {
        self.dir_generation = self.dir_generation.wrapping_add(1);
        self.active_local_refresh = None;
        self.dir_generation
    }

    /// Re-Use cached `LocalDirectory` entries when opening another file in the same folder,
    /// avoiding redundant directory `stat` storms on slow SMB/UNC network shares.
    pub fn cached_parent_scan(&self, path: &Path) -> Option<(PathBuf, ScanResult)> {
        self.local_dir
            .folder()
            .filter(|folder| path.parent() == Some(*folder))
            .map(|folder| {
                (
                    folder.to_path_buf(),
                    ScanResult {
                        entries: self.local_dir.entries().to_vec(),
                        subfolders: self.local_dir.subfolders().to_vec(),
                    },
                )
            })
    }

    /// Synchronizes `LocationStore` and catalog rows from a [`crate::PluginDomainChanges`] batch.
    pub fn apply_plugin_domain_changes(
        &mut self,
        d: crate::PluginDomainChanges,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) {
        if d.locations_reloaded {
            self.locations.reload();
        }
        if d.favorites_changed || d.catalog_changed {
            self.rebuild_rows(plugins, current_uri, current_local);
        }
    }
}

#[cfg(test)]
mod tests;
