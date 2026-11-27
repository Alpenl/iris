//! Source switching, plugin browse coordination, pinned bookmark actions,
//! cross-source [`PlayQueue`] stepping, direct stream validation, and external
//! path batch planning (including single-instance IPC deliveries).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use iris_base::net::http_client::validate_stream_url;
use iris_core::bookmarks::PinnedBookmark;
use iris_core::catalog::{CatalogItem, QueueItem};
use iris_core::library::{self, Entry, Kind};
use iris_core::media_uri::MediaUri;
use iris_core::player::RemoteMediaRequest;
use iris_core::preset::SidebarMode;
use iris_core::settings::Settings;

use super::CatalogController;
use crate::plugins::{
    ActiveSource, PluginBatchOutcome, PluginContext, PluginController, PluginPlayer,
    remote_search_query,
};

/// Resolved target action when opening a [`PinnedBookmark`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkAction {
    OpenLocal(PathBuf),
    BrowsePlugin {
        plugin_id: String,
        source_id: String,
        folder_id: Option<String>,
    },
    OpenDirectStream {
        url: String,
        title: Option<String>,
    },
}

/// Validated target for `open_direct_stream_url`.
#[derive(Clone, Debug)]
pub enum DirectStreamTarget {
    Empty,
    LocalPath(PathBuf),
    InvalidScheme,
    Remote(Box<RemoteMediaRequest>),
}

/// Planned media action for an external path batch (`open_paths` / single-instance IPC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenMediaBatchAction {
    None,
    SinglePath(PathBuf),
    MultiQueue(Vec<PathBuf>),
}

/// Deterministic partition of dropped, CLI, Finder `on_open_urls`, or Windows single-instance IPC paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenBatchPlan {
    pub package_to_review: Option<PathBuf>,
    pub media_action: OpenMediaBatchAction,
    pub subtitles: Vec<PathBuf>,
}

/// Outcome of unified `Prev` / `Next` navigation across `PlayQueue`, plugin playlists, and active catalog.
#[derive(Debug)]
pub enum StepAction {
    OpenMediaUri {
        uri: MediaUri,
        title: Option<String>,
    },
    OpenCatalogItem(CatalogItem),
    PluginPlaylist(PluginBatchOutcome),
    EndOfQueue,
    EndOfList,
    Noop,
}

/// Partitions an external path batch into at most one `.irisplugin` package, media paths, and subtitles.
pub fn plan_open_paths(paths: Vec<PathBuf>) -> OpenBatchPlan {
    let (packages, mut media_paths): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| {
        p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("irisplugin"))
    });
    let package_to_review = packages.into_iter().next();
    media_paths.retain(|path| !path.as_os_str().is_empty());
    let subtitles: Vec<PathBuf> = media_paths
        .extract_if(.., |path| library::is_subtitle(path))
        .collect();
    let media_action = match media_paths.as_slice() {
        [] => OpenMediaBatchAction::None,
        [single] => OpenMediaBatchAction::SinglePath(single.clone()),
        _ => OpenMediaBatchAction::MultiQueue(media_paths),
    };
    OpenBatchPlan {
        package_to_review,
        media_action,
        subtitles,
    }
}

/// Resolves local filesystem paths into playable [`QueueItem`]s (safe symlink/verbatim resolution).
pub fn resolve_queue_paths(paths: Vec<PathBuf>) -> Vec<QueueItem> {
    paths
        .into_iter()
        .map(|p| library::safe_resolve_path(&p))
        .filter_map(library::entry_from_path)
        .map(|e| QueueItem::from_local_entry(&e))
        .collect()
}

/// Parses and validates a user-supplied direct stream URL or local path fallback.
pub fn parse_direct_stream_input(raw_url: &str, title: Option<String>) -> DirectStreamTarget {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return DirectStreamTarget::Empty;
    }
    if !trimmed.contains("://") {
        return DirectStreamTarget::LocalPath(PathBuf::from(trimmed));
    }
    let Ok(parsed) = validate_stream_url(trimmed) else {
        return DirectStreamTarget::InvalidScheme;
    };
    let clean_url = parsed.to_string();
    let name = title.unwrap_or_else(|| {
        parsed
            .path_segments()
            .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| parsed.host_str().unwrap_or("网络媒体").to_owned())
    });
    let low = name.to_ascii_lowercase();
    let is_live = low.ends_with(".m3u8") || low.contains("live");
    let kind = library::kind_of(Path::new(&name)).unwrap_or(Kind::Video);
    DirectStreamTarget::Remote(Box::new(RemoteMediaRequest {
        uri: MediaUri::HttpUrl(clean_url.clone()),
        title: name,
        playback_url: clean_url,
        headers: BTreeMap::new(),
        kind,
        is_live,
        photo: None,
        subtitles: Vec::new(),
    }))
}

/// Returns `true` when `Prev` / `Next` (`delta`) can step `queue` to an entry different from `current`.
pub fn can_step_queue(
    queue: &iris_core::catalog::PlayQueue,
    current: Option<&MediaUri>,
    delta: isize,
) -> bool {
    match current {
        Some(current) => queue
            .step(current, delta, true)
            .is_some_and(|next| &next.uri != current),
        None => !queue.is_empty(),
    }
}

impl CatalogController {
    /// Closes any active plugin browse view, bumps `dir_generation`, switches to `ActiveSource::Local`
    /// (`SidebarMode::Files`), and rebuilds local rows.
    pub fn browse_local_source(
        &mut self,
        plugins: &mut PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) -> PluginBatchOutcome {
        self.bump_dir_generation();
        let out = plugins.close_browse(&mut self.active_source);
        self.active_source = ActiveSource::Local;
        self.sidebar_mode = SidebarMode::Files;
        self.rebuild_rows(plugins, current_uri, current_local);
        out
    }

    /// Closes any active plugin browse view and resets `active_source` to `Local`, bumping `dir_generation`
    /// so late browse pages or local refreshes cannot clobber the transition.
    pub fn close_plugin_browse(&mut self, plugins: &mut PluginController) -> PluginBatchOutcome {
        self.bump_dir_generation();
        let out = plugins.close_browse(&mut self.active_source);
        self.active_source = ActiveSource::Local;
        out
    }

    /// Initiates or paginates a `MediaSource` plugin browse request with monotonic generation fencing.
    #[allow(clippy::too_many_arguments)]
    pub fn browse_plugin_source(
        &mut self,
        plugins: &mut PluginController,
        plugin_id: &str,
        source_id: &str,
        folder_id: Option<String>,
        page: Option<String>,
        query: Option<String>,
        now: Instant,
    ) -> (bool, PluginBatchOutcome) {
        let is_new_view = page.is_none();
        if is_new_view {
            self.bump_dir_generation();
        }
        let (started, out) = plugins.browse_source(
            &mut self.active_source,
            plugin_id,
            source_id,
            folder_id,
            page,
            query,
            now,
        );
        if started && is_new_view {
            self.sidebar_mode = SidebarMode::Files;
        }
        (started, out)
    }

    /// Refreshes the currently browsed plugin source if it matches `(plugin_id, source_id)`.
    pub fn refresh_plugin_source(
        &mut self,
        plugins: &mut PluginController,
        plugin_id: &str,
        source_id: &str,
        search_open: bool,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        let ActiveSource::Plugin(view) = &self.active_source else {
            return None;
        };
        if view.plugin_id != plugin_id || view.source_id != source_id {
            return None;
        }
        let folder = view.folder_id.clone();
        let query = remote_search_query(view.supports_search && search_open, &self.search_query);
        Some(self.browse_plugin_source(plugins, plugin_id, source_id, folder, None, query, now))
    }

    /// Submits the current `search_query`: triggers remote plugin `browse` when the active plugin
    /// source supports server-side search, otherwise filters local/cached rows in place (`None`).
    pub fn submit_directory_search(
        &mut self,
        plugins: &mut PluginController,
        current_uri: Option<&MediaUri>,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        if let ActiveSource::Plugin(ps) = &self.active_source
            && ps.supports_search
        {
            let (pid, sid, fid) = (
                ps.plugin_id.clone(),
                ps.source_id.clone(),
                ps.folder_id.clone(),
            );
            let q = remote_search_query(true, &self.search_query);
            Some(self.browse_plugin_source(plugins, &pid, &sid, fid, None, q, now))
        } else {
            self.filter_rows(current_uri);
            None
        }
    }

    /// Updates a plugin source filter value and immediately triggers a fresh `browse` if a plugin source is active.
    pub fn select_plugin_source_filter(
        &mut self,
        plugins: &mut PluginController,
        filter_id: &str,
        value: Option<String>,
        search_open: bool,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        let ActiveSource::Plugin(view) = &self.active_source else {
            return None;
        };
        let (pid, sid, folder, supports_search) = (
            view.plugin_id.clone(),
            view.source_id.clone(),
            view.folder_id.clone(),
            view.supports_search,
        );
        plugins.set_source_filter_value(filter_id, value);
        let query = remote_search_query(supports_search && search_open, &self.search_query);
        Some(self.browse_plugin_source(plugins, &pid, &sid, folder, None, query, now))
    }

    /// Pins the currently browsed local folder or plugin source folder to `locations.json`.
    pub fn pin_current_location(&mut self) -> Result<PinnedBookmark, &'static str> {
        let (name, uri, folder_id) = match &self.active_source {
            ActiveSource::Local => {
                let Some(folder) = self.local_dir.folder().map(Path::to_path_buf) else {
                    return Err("当前未打开本地文件夹");
                };
                let name = folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.display().to_string());
                (name, MediaUri::Local(folder), None)
            }
            ActiveSource::Plugin(ps) => (
                ps.title.clone(),
                MediaUri::Plugin {
                    plugin_id: ps.plugin_id.clone(),
                    source_id: ps.source_id.clone(),
                    entry_id: ps.folder_id.clone().unwrap_or_default(),
                },
                ps.folder_id.clone(),
            ),
        };
        Ok(self.pin_bookmark(name, uri, folder_id))
    }

    /// Adds or updates a pinned bookmark with a deterministic `bm-{n}` identifier and saves `locations.json`.
    pub fn pin_bookmark(
        &mut self,
        name: String,
        uri: MediaUri,
        folder_id: Option<String>,
    ) -> PinnedBookmark {
        let id = (1..)
            .map(|n| format!("bm-{n}"))
            .find(|id| !self.locations.bookmarks.iter().any(|bm| &bm.id == id))
            .expect("finite bookmark list");
        let bookmark = PinnedBookmark {
            id,
            name,
            uri,
            folder_id,
        };
        self.locations.add_bookmark(bookmark.clone());
        bookmark
    }

    pub fn remove_bookmark(&mut self, id: &str) -> bool {
        self.locations.remove_bookmark(id)
    }

    pub fn reload_locations(&mut self) {
        self.locations.reload();
    }

    /// Updates `active_source` / `sidebar_mode` for opening `bm` and returns the concrete action to execute.
    pub fn prepare_open_bookmark(&mut self, bm: &PinnedBookmark) -> BookmarkAction {
        match &bm.uri {
            MediaUri::Local(path) => {
                self.bump_dir_generation();
                self.active_source = ActiveSource::Local;
                self.sidebar_mode = SidebarMode::Files;
                BookmarkAction::OpenLocal(path.clone())
            }
            MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            } => {
                self.sidebar_mode = SidebarMode::Files;
                let folder_id = bm
                    .folder_id
                    .clone()
                    .or_else(|| (!entry_id.is_empty()).then(|| entry_id.clone()));
                BookmarkAction::BrowsePlugin {
                    plugin_id: plugin_id.clone(),
                    source_id: source_id.clone(),
                    folder_id,
                }
            }
            MediaUri::HttpUrl(url) => BookmarkAction::OpenDirectStream {
                url: url.clone(),
                title: Some(bm.name.clone()),
            },
        }
    }

    pub fn push_queue_item(&mut self, item: QueueItem) -> bool {
        self.queue.push(item)
    }

    pub fn extend_queue_with_idle_check(
        &mut self,
        items: Vec<QueueItem>,
        player_idle: bool,
    ) -> (usize, Option<QueueItem>) {
        let first = items.first().cloned();
        let added = self.queue.extend(items);
        let auto_open = if player_idle { first } else { None };
        (added, auto_open)
    }

    pub fn extend_queue_from_catalog(&mut self) -> usize {
        self.queue.extend_catalog(&self.items)
    }

    pub fn replace_queue_from_entries(&mut self, entries: &[Entry]) {
        self.queue.replace_from_entries(entries);
    }

    pub fn remove_queue_uri(&mut self, uri: &MediaUri) -> bool {
        self.queue.remove(uri)
    }

    pub fn remove_queue_at(&mut self, index: usize) -> Option<QueueItem> {
        self.queue.remove_at(index)
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
    }

    pub fn can_step_media(
        &self,
        plugins: &PluginController,
        current: Option<&MediaUri>,
        delta: isize,
    ) -> bool {
        if !self.queue.is_empty() {
            return can_step_queue(&self.queue, current, delta);
        }
        self.playable_count > 1 || plugins.can_step_playlist(current, delta)
    }

    /// Unified `Prev` / `Next` navigation: steps `PlayQueue` first when non-empty,
    /// then active plugin playlist, and finally playable `CatalogItem`s in the active view.
    #[allow(clippy::too_many_arguments)]
    pub fn step<P: PluginPlayer>(
        &mut self,
        plugins: &mut PluginController,
        player: &mut P,
        settings: &Settings,
        current: Option<&MediaUri>,
        delta: isize,
        wrap: bool,
        now: Instant,
    ) -> StepAction {
        if !self.queue.is_empty() {
            let next_q = if let Some(u) = current {
                self.queue.step(u, delta, wrap).cloned()
            } else {
                let ix = if delta < 0 { self.queue.len() - 1 } else { 0 };
                self.queue.get(ix).cloned()
            };
            return match next_q {
                Some(q) => StepAction::OpenMediaUri {
                    uri: q.uri,
                    title: Some(q.title),
                },
                None => StepAction::EndOfQueue,
            };
        }

        let playlist_step = self.with_plugin_parts(|store, active_source| {
            let ctx = PluginContext {
                player,
                store,
                settings,
                active_source,
            };
            plugins.step_playlist(ctx, current, delta, wrap, now)
        });
        if let Some(out) = playlist_step {
            return StepAction::PluginPlaylist(out);
        }

        let playable: Vec<CatalogItem> = self
            .items
            .iter()
            .filter(|i| !i.is_folder())
            .cloned()
            .collect();
        if playable.is_empty() {
            return StepAction::Noop;
        }
        let cur_ix = current.and_then(|u| playable.iter().position(|i| &i.uri == u));
        let next_ix = match cur_ix {
            Some(i) => {
                let raw = i as isize + delta;
                if wrap {
                    Some(raw.rem_euclid(playable.len() as isize) as usize)
                } else if raw >= 0 && (raw as usize) < playable.len() {
                    Some(raw as usize)
                } else {
                    None
                }
            }
            None => Some(if delta < 0 { playable.len() - 1 } else { 0 }),
        };
        match next_ix {
            Some(ix) => StepAction::OpenCatalogItem(playable[ix].clone()),
            None => StepAction::EndOfList,
        }
    }
}
