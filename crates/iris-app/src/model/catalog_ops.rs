//! Directory catalog, cross-source queue, bookmarks, local/remote media open commits,
//! and `Store` progress persistence commands on [`super::AppModel`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use iris_base::net::http_client::HttpCancel;
use iris_core::bookmarks::PinnedBookmark;
use iris_core::catalog::{MediaFilter, QueueItem};
use iris_core::library::{Kind, ScanResult, SortBy, SortOrder};
use iris_core::media_uri::MediaUri;
use iris_core::player::{Prepared, PreparedQueue, RemoteMediaRequest, Session};

use super::AppModel;
use crate::catalog::{
    BookmarkAction, CompletedFavoriteScan, FavoriteScanJob, LocalRefreshTicket, StepAction,
    StoreFlushIntent, StoreFlushReceipt,
};
use crate::playback::{NowPlayingSnapshot, PlaybackSessionPort};
use crate::plugins::{PluginBatchOutcome, PluginPlayer};
use crate::workbench::WorkbenchDomainChanges;

/// Outcome of committing an asynchronous local, multi-queue, or direct remote media open.
#[derive(Debug)]
pub enum MediaOpenCommit {
    /// Stale `generation` fenced out by a newer load or cancellation; ignore silently.
    StaleGeneration,
    /// Media opened and catalog/plugin state synchronized (`queued_count` set for multi-queue opens).
    Opened {
        queued_count: Option<usize>,
        flush_idle_subtitles: bool,
    },
    /// Media open failed or multi-queue contained zero playable files.
    Failed {
        message: String,
        is_error_toast: bool,
        rebuild_rows: bool,
        now_playing: Option<Option<NowPlayingSnapshot>>,
    },
}

impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    pub fn has_media_load_pending(&self) -> bool {
        self.playback.pending_path().is_some()
            || self.catalog.has_remote_load()
            || self.plugins.has_pending_resolve()
    }

    /// Cancels any in-flight plugin `resolve` request, remote probe, or local prepare generation.
    pub fn cancel_in_flight_media_loads(&mut self) -> bool {
        let was_resolving = self.plugins.cancel_resolve_load();
        let was_remote = self.catalog.cancel_remote_load();
        self.playback
            .cancel_in_flight_load(was_resolving || was_remote)
    }

    pub fn cancel_remote_media_load(&mut self) -> bool {
        let was_remote = self.catalog.cancel_remote_load();
        self.playback.cancel_in_flight_load(was_remote)
    }

    pub fn save_current_progress_sync(&mut self, now: Instant) {
        let progress = PluginPlayer::progress(self.playback.session());
        self.catalog.save_progress_sync(
            progress,
            self.workbench.settings.resume_playback,
            self.plugins.scoped_released(),
            now,
        );
    }

    pub fn maybe_periodic_store_flush(&mut self, now: Instant) -> Option<StoreFlushIntent> {
        let progress = PluginPlayer::progress(self.playback.session());
        self.catalog.maybe_periodic_flush(
            progress,
            self.workbench.settings.resume_playback,
            self.plugins.scoped_released(),
            now,
        )
    }

    pub fn complete_store_flush(&mut self, receipt: StoreFlushReceipt) -> Option<StoreFlushIntent> {
        self.catalog.complete_store_flush(receipt)
    }

    /// Synchronizes plugin stream ownership, catalog rows, recent history, and initial photo view
    /// mode immediately after a local, remote, or plugin-admitted media load succeeds.
    pub fn finish_media_open(&mut self) {
        self.plugins.sync_media_open_state(self.playback.session());
        let (cur_uri, share_identity, title) = {
            let session = self.playback.session();
            (
                PlaybackSessionPort::current_media_uri(session),
                PlaybackSessionPort::may_share_current_identity(session),
                PlaybackSessionPort::current_title(session).map(str::to_owned),
            )
        };
        self.catalog
            .record_media_opened(cur_uri, share_identity, title.as_deref());
        self.catalog
            .rebuild_rows_for_player(&self.plugins, self.playback.session());
        self.playback
            .on_media_open_succeeded(self.workbench.settings.photo_default);
    }

    pub fn begin_local_path_open(&mut self, path: &Path) -> (u64, Option<(PathBuf, ScanResult)>) {
        self.plugins.clear_active_stream_meta();
        let cached = self.catalog.cached_parent_scan(path);
        let generation = self.playback.begin_local_load(Some(path.to_path_buf()));
        (generation, cached)
    }

    pub fn begin_local_batch_open(&mut self, first: Option<PathBuf>) -> u64 {
        self.plugins.clear_active_stream_meta();
        self.playback.begin_local_load(first)
    }

    pub fn begin_remote_stream_open(&mut self) -> (u64, HttpCancel, Arc<futures::lock::Mutex<()>>) {
        let generation = self.playback.begin_remote_load();
        let (cancel, slot) = self.catalog.begin_remote_load();
        (generation, cancel, slot)
    }

    pub fn prepare_direct_remote_entry(&mut self) -> PluginBatchOutcome {
        let out = self.catalog.close_plugin_browse(&mut self.plugins);
        self.plugins.clear_active_stream_meta();
        self.rebuild_directory_rows();
        out
    }

    pub fn step_media(&mut self, delta: isize, wrap: bool, now: Instant) -> StepAction {
        let cur = self
            .playback
            .pending_path()
            .map(Path::to_path_buf)
            .map(MediaUri::Local)
            .or_else(|| PlaybackSessionPort::current_media_uri(self.playback.session()));
        self.catalog.step(
            &mut self.plugins,
            self.playback.session_mut(),
            &self.workbench.settings,
            cur.as_ref(),
            delta,
            wrap,
            now,
        )
    }

    pub fn can_step_media(&self, delta: isize) -> bool {
        let cur = self
            .playback
            .pending_path()
            .map(Path::to_path_buf)
            .map(MediaUri::Local)
            .or_else(|| PlaybackSessionPort::current_media_uri(self.playback.session()));
        self.catalog
            .can_step_media(&self.plugins, cur.as_ref(), delta)
    }

    pub fn cycle_sort(&mut self) -> (SortBy, SortOrder, WorkbenchDomainChanges) {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        let cur_local = PlaybackSessionPort::current(self.playback.session());
        let (next_by, next_ord) =
            self.catalog
                .cycle_sort(&self.plugins, cur_uri.as_ref(), cur_local);
        let changes = self.workbench.set_sort_preference(next_by, next_ord);
        (next_by, next_ord, changes)
    }

    pub fn set_filter(&mut self, filter: MediaFilter) {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        self.catalog.set_filter(filter, cur_uri.as_ref());
    }

    pub fn set_search_query(&mut self, query: String) {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        self.catalog.set_search_query(query, cur_uri.as_ref());
    }

    pub fn reset_search_and_rebuild(&mut self, query: String) {
        self.catalog.set_search_query(query, None);
        self.rebuild_directory_rows();
    }

    pub fn rebuild_directory_rows(&mut self) {
        self.catalog
            .rebuild_rows_for_player(&self.plugins, self.playback.session());
    }

    pub fn submit_directory_search(
        &mut self,
        query: String,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        self.catalog.set_search_query(query, cur_uri.as_ref());
        self.catalog
            .submit_directory_search(&mut self.plugins, cur_uri.as_ref(), now)
    }

    pub fn refresh_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        query: String,
        search_open: bool,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        self.set_search_query(query);
        self.catalog.refresh_plugin_source(
            &mut self.plugins,
            plugin_id,
            source_id,
            search_open,
            now,
        )
    }

    pub fn select_plugin_source_filter(
        &mut self,
        query: String,
        filter_id: &str,
        value: Option<String>,
        search_open: bool,
        now: Instant,
    ) -> Option<(bool, PluginBatchOutcome)> {
        self.set_search_query(query);
        self.catalog.select_plugin_source_filter(
            &mut self.plugins,
            filter_id,
            value,
            search_open,
            now,
        )
    }

    pub fn browse_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        folder_id: Option<String>,
        page: Option<String>,
        query: Option<String>,
        now: Instant,
    ) -> (bool, PluginBatchOutcome) {
        self.catalog.browse_plugin_source(
            &mut self.plugins,
            plugin_id,
            source_id,
            folder_id,
            page,
            query,
            now,
        )
    }

    pub fn browse_local_source(&mut self) -> PluginBatchOutcome {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        let cur_local = PlaybackSessionPort::current(self.playback.session());
        self.catalog
            .browse_local_source(&mut self.plugins, cur_uri.as_ref(), cur_local)
    }

    pub fn close_plugin_browse(&mut self) -> PluginBatchOutcome {
        self.catalog.close_plugin_browse(&mut self.plugins)
    }

    pub fn begin_local_directory_refresh(&mut self) -> Option<(LocalRefreshTicket, u64)> {
        if self.playback.pending_path().is_some() {
            return None;
        }
        let ticket = self.catalog.begin_local_directory_refresh()?;
        Some((ticket, self.playback.load_generation()))
    }

    pub fn finish_local_directory_refresh(
        &mut self,
        ticket: &LocalRefreshTicket,
        scan: ScanResult,
        load_gen_at_start: u64,
    ) -> bool {
        let media_pending = self.playback.load_generation() != load_gen_at_start
            || self.playback.pending_path().is_some();
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        let cur_local = PlaybackSessionPort::current(self.playback.session());
        self.catalog.finish_local_directory_refresh(
            ticket,
            scan,
            media_pending,
            &self.plugins,
            cur_uri.as_ref(),
            cur_local,
        )
    }

    pub fn begin_favorite_scan(&mut self) -> Option<FavoriteScanJob> {
        self.catalog.begin_favorite_scan()
    }

    pub fn finish_favorite_scan(&mut self, completed: CompletedFavoriteScan) -> bool {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        self.catalog
            .finish_favorite_scan(completed, cur_uri.as_ref())
    }

    pub fn toggle_favorite(&mut self, uri: &MediaUri) -> bool {
        let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
        let cur_local = PlaybackSessionPort::current(self.playback.session());
        self.catalog
            .toggle_favorite(uri, &self.plugins, cur_uri.as_ref(), cur_local)
    }

    pub fn toggle_current_favorite(&mut self) -> Option<(MediaUri, bool)> {
        let uri = PlaybackSessionPort::current_media_uri(self.playback.session())?;
        let cur_local = PlaybackSessionPort::current(self.playback.session());
        let now_fav = self
            .catalog
            .toggle_favorite(&uri, &self.plugins, Some(&uri), cur_local);
        Some((uri, now_fav))
    }

    pub fn reset_progress(&mut self, uri: &MediaUri) {
        self.catalog.reset_progress(uri);
    }

    pub fn clear_recent(&mut self) {
        self.catalog.clear_recent();
    }

    pub fn clear_progress(&mut self) -> usize {
        self.catalog.clear_progress()
    }

    pub fn pin_current_location(&mut self) -> std::result::Result<PinnedBookmark, &'static str> {
        self.catalog.pin_current_location()
    }

    pub fn pin_bookmark(&mut self, name: String, uri: MediaUri, folder_id: Option<String>) {
        self.catalog.pin_bookmark(name, uri, folder_id);
    }

    pub fn remove_bookmark(&mut self, id: &str) -> bool {
        self.catalog.remove_bookmark(id)
    }

    pub fn prepare_open_bookmark(&mut self, bm: &PinnedBookmark) -> BookmarkAction {
        self.catalog.prepare_open_bookmark(bm)
    }

    pub fn clear_queue(&mut self) {
        self.catalog.clear_queue();
    }

    pub fn remove_queue_at(&mut self, ix: usize) -> Option<QueueItem> {
        self.catalog.remove_queue_at(ix)
    }

    pub fn remove_queue_uri(&mut self, uri: &MediaUri) -> bool {
        self.catalog.remove_queue_uri(uri)
    }

    pub fn push_queue_item(&mut self, item: QueueItem) -> bool {
        self.catalog.push_queue_item(item)
    }

    pub fn extend_queue_from_catalog(&mut self) -> usize {
        self.catalog.extend_queue_from_catalog()
    }

    pub fn extend_queue_with_idle_check(
        &mut self,
        items: Vec<QueueItem>,
        idle: bool,
    ) -> (usize, Option<QueueItem>) {
        self.catalog.extend_queue_with_idle_check(items, idle)
    }

    pub fn prioritized_filmstrip_window(&self) -> Vec<PathBuf> {
        let is_photo = PlaybackSessionPort::kind(self.playback.session()) == Some(Kind::Photo);
        self.catalog
            .prioritized_filmstrip_window(is_photo, self.workbench.settings.photo_filmstrip)
    }
}

impl AppModel<Session> {
    pub fn commit_prepared_local_open(
        &mut self,
        generation: u64,
        prepared: Result<Prepared>,
        now: Instant,
    ) -> MediaOpenCommit {
        if !self.playback.accept_prepared_load(generation) {
            return MediaOpenCommit::StaleGeneration;
        }
        let result = prepared.and_then(|prepared| {
            let resume = self
                .catalog
                .resume_for_local(self.workbench.settings.resume_playback, prepared.target());
            let dir = self.playback.session_mut().open(prepared, resume)?;
            self.catalog.install_local_directory(dir);
            Ok(())
        });
        match result {
            Ok(()) => {
                self.finish_media_open();
                MediaOpenCommit::Opened {
                    queued_count: None,
                    flush_idle_subtitles: self.playback.session().kind().is_none(),
                }
            }
            Err(error) => {
                let now_playing = self.playback.on_media_open_failed(now);
                self.rebuild_directory_rows();
                MediaOpenCommit::Failed {
                    message: format!("无法打开 · {error}"),
                    is_error_toast: false,
                    rebuild_rows: true,
                    now_playing,
                }
            }
        }
    }

    pub fn commit_prepared_batch_open(
        &mut self,
        generation: u64,
        prepared: Option<PreparedQueue>,
        now: Instant,
    ) -> MediaOpenCommit {
        if !self.playback.accept_prepared_load(generation) {
            return MediaOpenCommit::StaleGeneration;
        }
        let Some(prepared) = prepared else {
            let now_playing = self.playback.on_media_open_failed(now);
            self.rebuild_directory_rows();
            return MediaOpenCommit::Failed {
                message: "所选项目中没有可播放的媒体".into(),
                is_error_toast: false,
                rebuild_rows: true,
                now_playing,
            };
        };
        let count = prepared.entries().len();
        self.catalog.replace_queue_from_entries(prepared.entries());
        let resume = self
            .catalog
            .resume_for_local(self.workbench.settings.resume_playback, prepared.target());
        match self
            .playback
            .session_mut()
            .open_prepared_queue(prepared, resume)
        {
            Ok(dir) => {
                self.catalog.install_local_directory(dir);
                self.finish_media_open();
                MediaOpenCommit::Opened {
                    queued_count: Some(count),
                    flush_idle_subtitles: false,
                }
            }
            Err(error) => {
                let now_playing = self.playback.on_media_open_failed(now);
                self.rebuild_directory_rows();
                MediaOpenCommit::Failed {
                    message: format!("无法打开 · {error}"),
                    is_error_toast: false,
                    rebuild_rows: true,
                    now_playing,
                }
            }
        }
    }

    pub fn commit_prepared_remote_open(
        &mut self,
        generation: u64,
        req: RemoteMediaRequest,
        now: Instant,
    ) -> MediaOpenCommit {
        if !self.playback.accept_prepared_load(generation) {
            return MediaOpenCommit::StaleGeneration;
        }
        self.catalog.cancel_remote_load();
        let resume = self.catalog.resume_for_remote(
            self.workbench.settings.resume_playback,
            req.is_live,
            req.kind,
            &req.uri,
        );
        let settings = self.workbench.settings.clone();
        match self
            .playback
            .session_mut()
            .open_remote(req, &settings, resume)
        {
            Ok(()) => {
                self.finish_media_open();
                MediaOpenCommit::Opened {
                    queued_count: None,
                    flush_idle_subtitles: false,
                }
            }
            Err(error) => {
                log::warn!("open_remote failed: {error:#}");
                let now_playing = self.playback.on_media_open_failed(now);
                MediaOpenCommit::Failed {
                    message: "无法打开远程流，请检查地址与网络状态".into(),
                    is_error_toast: true,
                    rebuild_rows: false,
                    now_playing,
                }
            }
        }
    }
}
