//! Media-record persistence (`Store`), periodic/async [`StoreFlushIntent`] +
//! [`StoreFlushReceipt`] state machine, write-failure dirty-bit retention,
//! mutation-during-flush continuation, private-identity filtering, and sync shutdown flush.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use iris_api::v2::ErrorCode;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::store::{AnnotationEdit, AnnotationSnapshot, ProgressFlush};

use super::CatalogController;
use crate::plugins::{
    DEFAULT_PLUGIN_MEDIA_TITLE, PendingHostAnnotation, PlayerProgress, PluginBatchOutcome,
    PluginController, PluginPlayer, should_record_progress,
};

/// Interval between periodic background progress flushes during active playback.
pub const SAVE_EVERY: Duration = Duration::from_secs(5);

/// Background `records.tsv` flush intent emitted by [`CatalogController`].
///
/// Executed off the UI thread via [`Self::run`], producing a [`StoreFlushReceipt`]
/// handed back to [`CatalogController::complete_store_flush`].
pub struct StoreFlushIntent {
    id: u64,
    flush: ProgressFlush,
}

impl StoreFlushIntent {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn run(self) -> StoreFlushReceipt {
        StoreFlushReceipt {
            id: self.id,
            applied: self.flush.run(),
        }
    }
}

/// Completion receipt returned by [`StoreFlushIntent::run`].
#[derive(Clone, Debug, PartialEq)]
pub struct StoreFlushReceipt {
    id: u64,
    applied: Option<BTreeMap<String, (f64, f64)>>,
}

impl StoreFlushReceipt {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn succeeded(&self) -> bool {
        self.applied.is_some()
    }
}

#[derive(Default, Debug)]
pub(super) struct StoreFlushState {
    next_id: u64,
    in_flight_id: Option<u64>,
    mutated_during_flush: bool,
}

impl CatalogController {
    pub fn is_store_flush_in_flight(&self) -> bool {
        self.flush_state.in_flight_id.is_some()
    }

    pub fn mutated_during_store_flush(&self) -> bool {
        self.flush_state.mutated_during_flush
    }

    /// Reconciles `flush_state` after a synchronous `Store` operation (`save()`, plugin command,
    /// or annotation commit).
    pub(crate) fn reconcile_plugin_store_edits(&mut self) {
        if self.flush_state.in_flight_id.is_some() {
            if !self.store.is_dirty() {
                // Synchronous `Store::save()` already aborted `in_flight_abort` and wrote all edits.
                self.flush_state.in_flight_id = None;
                self.flush_state.mutated_during_flush = false;
            } else {
                self.flush_state.mutated_during_flush = true;
            }
        }
    }

    /// Records `progress` into in-memory `Store` edits when `resume_playback && !scoped_released`.
    pub fn record_progress(
        &mut self,
        progress: Option<PlayerProgress>,
        resume_playback: bool,
        scoped_released: bool,
    ) -> bool {
        if !should_record_progress(resume_playback, scoped_released) {
            return false;
        }
        let Some(progress) = progress else {
            return false;
        };
        let changed = match progress {
            PlayerProgress::Local {
                path,
                pos,
                dur,
                eof,
            } => self.store.remember_path(&path, pos, dur, eof),
            PlayerProgress::Uri { uri, pos, dur, eof } => {
                let parsed = MediaUri::parse(&uri);
                self.store.remember(&parsed, pos, dur, eof)
            }
        };
        if changed && self.flush_state.in_flight_id.is_some() && self.store.is_dirty() {
            self.flush_state.mutated_during_flush = true;
        }
        changed
    }

    /// Evaluates whether `SAVE_EVERY` has elapsed since `last_save`; if so, records `progress`
    /// and initiates a [`StoreFlushIntent`] (or falls back to synchronous `Store::save()` when
    /// non-progress edits such as favorites/recent/legacy migration require full sync).
    pub fn maybe_periodic_flush(
        &mut self,
        progress: Option<PlayerProgress>,
        resume_playback: bool,
        scoped_released: bool,
        now: Instant,
    ) -> Option<StoreFlushIntent> {
        if now.saturating_duration_since(self.last_save) < SAVE_EVERY {
            return None;
        }
        self.record_progress(progress, resume_playback, scoped_released);
        self.last_save = now;
        self.begin_store_flush()
    }

    /// Starts a background [`StoreFlushIntent`] if `Store` is dirty and no flush is currently in flight.
    ///
    /// - If a flush is already in flight while `Store` is dirty, marks `mutated_during_flush` so
    ///   [`Self::complete_store_flush`] automatically chains a follow-up flush.
    /// - If `Store` has non-progress edits (favorites, recent list, clear-all, or legacy file migration),
    ///   performs synchronous `Store::save()` (retaining dirty bits if disk I/O fails).
    pub fn begin_store_flush(&mut self) -> Option<StoreFlushIntent> {
        if !self.store.is_dirty() {
            return None;
        }
        if self.flush_state.in_flight_id.is_some() {
            self.flush_state.mutated_during_flush = true;
            return None;
        }
        let Some(flush) = self.store.prepare_progress_flush() else {
            self.store.save();
            return None;
        };
        self.flush_state.next_id = self.flush_state.next_id.wrapping_add(1);
        let id = self.flush_state.next_id;
        self.flush_state.in_flight_id = Some(id);
        self.flush_state.mutated_during_flush = false;
        Some(StoreFlushIntent { id, flush })
    }

    /// Applies a [`StoreFlushReceipt`] from a completed background [`StoreFlushIntent`]:
    ///
    /// - Stale receipts (superseded by synchronous `save()` or `flush_on_shutdown()`) are ignored.
    /// - Failed writes (`!receipt.succeeded()`) retain all dirty bits in `Store` without immediately
    ///   spin-retrying, so the next periodic tick or shutdown retries cleanly.
    /// - Successful writes that observed concurrent mutations (`mutated_during_flush`) immediately
    ///   emit a follow-up [`StoreFlushIntent`] (or sync save) so new edits are persisted promptly.
    pub fn complete_store_flush(&mut self, receipt: StoreFlushReceipt) -> Option<StoreFlushIntent> {
        if self.flush_state.in_flight_id != Some(receipt.id) {
            return None;
        }
        self.flush_state.in_flight_id = None;
        let had_concurrent_mutation = self.flush_state.mutated_during_flush;
        self.flush_state.mutated_during_flush = false;

        let succeeded = receipt.succeeded();
        self.store.finish_progress_flush(receipt.applied);
        if succeeded && had_concurrent_mutation && self.store.is_dirty() {
            self.begin_store_flush()
        } else {
            None
        }
    }

    /// Synchronously records `progress` and flushes any dirty `Store` state (used before media
    /// switches and at EOF). If disk I/O fails, `Store::is_dirty()` remains `true` for retry.
    pub fn save_progress_sync(
        &mut self,
        progress: Option<PlayerProgress>,
        resume_playback: bool,
        scoped_released: bool,
        now: Instant,
    ) {
        self.record_progress(progress, resume_playback, scoped_released);
        self.flush_state.in_flight_id = None;
        self.flush_state.mutated_during_flush = false;
        if self.store.is_dirty() {
            self.store.save();
        }
        self.last_save = now;
    }

    /// Guarantees synchronous `Store` flush during window/application shutdown, invalidating any
    /// in-flight async flush receipt and writing all pending favorites, progress, and recent entries.
    pub fn flush_on_shutdown(
        &mut self,
        progress: Option<PlayerProgress>,
        resume_playback: bool,
        scoped_released: bool,
        now: Instant,
    ) {
        self.save_progress_sync(progress, resume_playback, scoped_released, now);
    }

    /// Records a newly opened media item or local folder into `recent.tsv` while strictly
    /// suppressing private-identity streams and tokenized HTTP URLs (`!uri.may_share_identity()`).
    pub fn record_media_opened(
        &mut self,
        current_uri: Option<MediaUri>,
        may_share_identity: bool,
        current_title: Option<&str>,
    ) {
        if let Some(uri) = current_uri {
            if may_share_identity {
                let title = match &uri {
                    MediaUri::Local(_) => None,
                    MediaUri::Plugin { .. } | MediaUri::HttpUrl(_) => current_title
                        .map(str::trim)
                        .filter(|t| !t.is_empty() && *t != DEFAULT_PLUGIN_MEDIA_TITLE)
                        .map(str::to_owned),
                };
                self.store.push_recent_with_title(&uri, title.as_deref());
                self.reconcile_plugin_store_edits();
            }
        } else if let Some(dir) = self.local_dir.folder().map(Path::to_path_buf)
            && self.active_source.is_local()
        {
            self.store.push_recent_path(&dir);
            self.reconcile_plugin_store_edits();
        }
    }

    /// Toggles favorite status for `uri` in `Store` and atomically rebuilds catalog rows/counts.
    pub fn toggle_favorite(
        &mut self,
        uri: &MediaUri,
        plugins: &PluginController,
        current_uri: Option<&MediaUri>,
        current_local: Option<&Path>,
    ) -> bool {
        let added = self.store.toggle_favorite(uri);
        self.reconcile_plugin_store_edits();
        self.rebuild_rows(plugins, current_uri, current_local);
        added
    }

    pub fn reset_progress(&mut self, uri: &MediaUri) -> bool {
        let reset = self.store.reset_progress(uri);
        self.reconcile_plugin_store_edits();
        reset
    }

    pub fn clear_progress(&mut self) -> usize {
        let cleared = self.store.clear_progress();
        self.reconcile_plugin_store_edits();
        cleared
    }

    pub fn clear_recent(&mut self) {
        self.store.clear_recent();
        self.reconcile_plugin_store_edits();
    }

    pub fn resume_for_local(&self, resume_playback: bool, target: Option<&Path>) -> f64 {
        if resume_playback {
            target.map_or(0., |p| self.store.resume_path(p))
        } else {
            0.
        }
    }

    pub fn resume_for_remote(
        &self,
        resume_playback: bool,
        is_live: bool,
        kind: Kind,
        uri: &MediaUri,
    ) -> f64 {
        if resume_playback && !is_live && kind == Kind::Video {
            self.store.resume(uri)
        } else {
            0.
        }
    }

    pub fn begin_host_annotation<P: PluginPlayer>(
        &self,
        plugins: &mut PluginController,
        player: &P,
        edit: Option<AnnotationEdit>,
    ) -> Result<Option<PendingHostAnnotation>, PluginBatchOutcome> {
        plugins.begin_host_annotation(player, &self.store, edit)
    }

    pub fn finish_host_annotation(
        &mut self,
        plugins: &mut PluginController,
        saving: bool,
        result: Result<AnnotationSnapshot, ErrorCode>,
    ) -> (bool, PluginBatchOutcome) {
        let out = plugins.finish_host_annotation(&mut self.store, saving, result);
        self.reconcile_plugin_store_edits();
        out
    }
}
