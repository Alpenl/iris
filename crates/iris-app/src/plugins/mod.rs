//! Headless [`PluginController`] vertical slice: single owner of `PluginHost`,
//! `WindowSession`, modal submit state, source browse/resolve correlation, stream
//! playlist & variant metadata, host consent sheets, and enhancement/overlay state.

mod commands;
mod consent;
mod dispatch;
mod enhance;
mod lifecycle;
mod modal;
mod ports;
mod sources;
mod thumbnails;

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id,
    enhance::{HeatRange, SkipRange},
    source::{PlaybackSnapshot, PlaylistEntry, SourceDescriptor, SourceEntry, SourceFilter},
};
use iris_api::{
    ChromeLayoutSpec, CommandSpec, OverlayNode, PluginModal, SidebarPanel, SourceProviderSpec,
    StatusBadge, ToolbarButton, UiThemeSpec,
};
use iris_core::store::Store;
use iris_plugin_host::{
    OriginSummary, OverlayFrame, PluginHost, PluginInfo, ReviewSummary, WindowSession,
};

pub use consent::REVIEW_EXPIRY;
pub use enhance::{PendingHostAnnotation, can_seek_skip_range};
pub use modal::{PluginModalState, ViewTimerSlot, run_view_timer};
pub use ports::{
    DEFAULT_PLUGIN_MEDIA_TITLE, OpenUrlOutcome, PlayerProgress, PluginBatchOutcome, PluginContext,
    PluginDomainChanges, PluginPlayer, PluginSideEffect, error_code_label,
};
pub use sources::{
    ActiveSource, ActiveStreamMeta, PendingBrowseRequest, PendingResolveRequest, PluginSourceView,
    SOURCE_TIMEOUT, SourceErrorKind, StreamReply, find_plugin_playlist_index, remote_search_query,
    resolve_playlist_current, should_record_progress, should_suppress_playback_error_toast,
};
pub use thumbnails::{
    MAX_REMOTE_THUMB_CONCURRENT, QUOTA_RETRY_COOLDOWN, StartedThumbnailFetch, ThumbKey,
    ThumbnailCache,
};

use consent::HostConsentState;
use enhance::{AnnotationJobState, step_overlay_frame_inner, timeline_ranges_view};
use modal::{resolve_modal_timer, resolve_sidebar_timers};
use sources::{BrowseState, StreamPlaybackState};

const TICK_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Single authoritative owner of plugin orchestration state across an application window.
pub struct PluginController {
    host: PluginHost,
    window: WindowSession,
    last_tick: Instant,
    deferred: Vec<(String, String)>,
    browse: BrowseState,
    stream: StreamPlaybackState,
    consent: HostConsentState,
    modal: PluginModalState,
    overlay_frame: Option<Arc<OverlayFrame>>,
    annotations: AnnotationJobState,
}

impl PluginController {
    /// Opens a window session against `host`, seeding initial theme overrides and deferred commands.
    pub fn open_window(
        host: PluginHost,
        disabled_plugins: &[String],
        theme_id: &str,
        deferred: Vec<(String, String)>,
        now: Instant,
    ) -> (Self, UnboundedReceiver<()>) {
        let (window, wake_rx) = host.open_window(disabled_plugins, theme_id);
        let mut modal = PluginModalState::default();
        if window.active_modal().is_some() {
            modal.mark_focus_pending();
        }
        (
            Self {
                host,
                window,
                last_tick: now,
                deferred,
                browse: BrowseState::default(),
                stream: StreamPlaybackState::default(),
                consent: HostConsentState::default(),
                modal,
                overlay_frame: None,
                annotations: AnnotationJobState::default(),
            },
            wake_rx,
        )
    }

    /// Shuts down the active window session before tearing down the underlying `PluginHost`.
    pub fn shutdown(&self) {
        self.window.shutdown();
        self.host.shutdown();
    }

    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    pub fn window(&self) -> &WindowSession {
        &self.window
    }

    pub fn list(&self) -> &[PluginInfo] {
        self.window.list()
    }

    pub fn is_ready(&self, plugin_id: &str) -> bool {
        self.window.is_ready(plugin_id)
    }

    pub fn generation(&self) -> Generation {
        self.window.generation()
    }

    pub fn has_state_listeners(&self) -> bool {
        self.window.has_state_listeners()
    }

    pub fn source_providers(&self) -> Vec<(String, SourceProviderSpec)> {
        self.window.source_providers()
    }

    pub fn media_sources(&self) -> Vec<(&str, &str, &SourceDescriptor)> {
        self.window.media_sources()
    }

    pub fn commands(&self) -> Vec<(&str, &str, &CommandSpec)> {
        self.window.commands()
    }

    pub fn url_resolvers(&self, url: Option<&str>) -> Vec<(String, String)> {
        self.window.url_resolvers(url)
    }

    pub fn sidebar_panels(&self) -> Vec<(&str, &SidebarPanel)> {
        self.window.sidebar_panels()
    }

    pub fn status_badges(&self) -> Vec<(&str, &StatusBadge)> {
        self.window.status_badges()
    }

    pub fn toolbar_buttons(&self) -> Vec<(&str, &ToolbarButton)> {
        self.window.toolbar_buttons()
    }

    pub fn overlay_nodes(&self) -> impl Iterator<Item = (&str, &OverlayNode)> {
        self.window.overlay_nodes()
    }

    pub fn active_modal(&self) -> Option<(&str, &PluginModal)> {
        self.window.active_modal()
    }

    pub fn active_ui_overrides(&self) -> (Option<UiThemeSpec>, Option<ChromeLayoutSpec>) {
        self.window.active_ui_overrides()
    }

    pub fn select_theme(
        &mut self,
        theme_id: &str,
    ) -> (Option<UiThemeSpec>, Option<ChromeLayoutSpec>) {
        let desired = theme_id.strip_prefix("plugin:").map(str::to_owned);
        let _ = self.window.select_theme(desired);
        self.window.active_ui_overrides()
    }

    pub fn modal(&self) -> &PluginModalState {
        &self.modal
    }

    pub fn modal_mut(&mut self) -> &mut PluginModalState {
        &mut self.modal
    }

    pub fn has_host_consent(&self) -> bool {
        self.consent.has_active()
    }

    pub fn review_summary(&self) -> Option<&Arc<ReviewSummary>> {
        self.consent.review.as_ref()
    }

    pub fn origin_summary(&self) -> Option<&Arc<OriginSummary>> {
        self.consent.origin.as_ref()
    }

    pub fn active_consent_id(&self) -> Option<Id> {
        self.consent.active_id()
    }

    pub fn is_consent_busy(&self, id: Id) -> bool {
        self.consent.is_busy(id)
    }

    pub fn insecure_auth(&self) -> bool {
        self.consent.insecure_auth
    }

    pub fn toggle_insecure_auth(&mut self, origin_id: Id) -> bool {
        self.consent.toggle_insecure_auth(origin_id)
    }

    pub fn keep_identity(&self) -> bool {
        self.consent.keep_identity
    }

    pub fn toggle_keep_identity(&mut self, review_id: Id) -> bool {
        self.consent.toggle_keep_identity(review_id)
    }

    /// Returns `(plugin_id, action_id, clamped_interval_ms)` for the active modal's timer when unblocked.
    pub fn active_modal_timer(&self) -> Option<(String, String, u32)> {
        resolve_modal_timer(self.has_host_consent(), self.window.active_modal())
    }

    /// Returns `(plugin_id, action_id, clamped_interval_ms)` for all active sidebar panels when unblocked.
    pub fn sidebar_panel_timers(&self) -> Vec<(String, String, u32)> {
        let panels = self.window.sidebar_panels();
        resolve_sidebar_timers(self.has_host_consent(), self.window.active_modal(), &panels)
    }

    pub fn source_entries<'a>(
        &'a self,
        source_id: &'a str,
    ) -> impl Iterator<Item = &'a SourceEntry> {
        self.browse.entries(source_id)
    }

    pub fn source_filters(&self) -> &[SourceFilter] {
        &self.browse.source_filters
    }

    pub fn set_source_filter_value(&mut self, filter_id: &str, value: Option<String>) {
        if let Some(f) = self
            .browse
            .source_filters
            .iter_mut()
            .find(|f| f.id == filter_id)
        {
            f.selected = value.clone();
        }
        if let Some(val) = value {
            self.browse.filter_values.insert(filter_id.to_owned(), val);
        } else {
            self.browse.filter_values.remove(filter_id);
        }
    }

    pub fn active_browse_view(&self) -> Option<&(String, Id)> {
        self.browse.view.as_ref()
    }

    pub fn page_binding(&self) -> Option<&Arc<iris_plugin_host::Binding>> {
        self.browse.page_binding.as_ref()
    }

    pub fn pages(&self) -> &[Arc<iris_plugin_host::AcceptedPage>] {
        &self.browse.pages
    }

    pub fn has_pending_resolve(&self) -> bool {
        self.browse.pending_resolve.is_some()
    }

    pub fn has_pending_browse(&self) -> bool {
        self.browse.pending_browse.is_some()
    }

    pub fn active_stream_meta(&self) -> Option<&ActiveStreamMeta> {
        self.stream.active_stream_meta.as_ref()
    }

    pub fn clear_active_stream_meta(&mut self) {
        self.stream.active_stream_meta = None;
    }

    pub fn scoped_released(&self) -> bool {
        self.stream.scoped_released
    }

    pub fn playlist(&self) -> &[PlaylistEntry] {
        &self.stream.playlist
    }

    pub fn overlay_frame(&self) -> Option<&Arc<OverlayFrame>> {
        self.overlay_frame.as_ref()
    }

    pub fn timeline_ranges(&self) -> Option<(&[HeatRange], &[SkipRange])> {
        timeline_ranges_view(&self.window)
    }

    pub fn playback_snapshot<P: PluginPlayer>(
        &self,
        player: &P,
        store: &Store,
    ) -> PlaybackSnapshot {
        let fav = player
            .current_media_uri()
            .is_some_and(|u| store.is_favorite(&u));
        self.window
            .playback_snapshot(&player.playback_snapshot_input(), fav)
    }

    pub fn bind_current_file<P: PluginPlayer>(&self, player: &P) {
        let path = player.current_local_path().map(std::path::Path::to_owned);
        self.window.set_current_local_file(path);
    }

    pub fn precheck_capability(
        &self,
        plugin_id: &str,
        capability: Capability,
    ) -> Result<(), ErrorCode> {
        self.window.precheck_capability(plugin_id, capability)
    }

    pub fn source_badge(&self, plugin_id: &str, source_id: &str) -> String {
        let sources = self.window.media_sources();
        let found = sources
            .iter()
            .copied()
            .find(|(pid, _, desc)| *pid == plugin_id && desc.id == source_id)
            .or_else(|| {
                sources
                    .iter()
                    .copied()
                    .find(|(pid, _, _)| *pid == plugin_id)
            });
        if let Some((_, pname, desc)) = found {
            if let Some(badge) = desc
                .badge
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
            {
                return badge.to_owned();
            }
            if !pname.trim().is_empty() {
                return pname.to_owned();
            }
        }
        "插件源".into()
    }

    pub fn source_icon_name(&self, plugin_id: &str, source_id: &str) -> Option<String> {
        let sources = self.window.media_sources();
        sources
            .iter()
            .find(|(pid, _, desc)| *pid == plugin_id && desc.id == source_id)
            .or_else(|| sources.iter().find(|(pid, _, _)| *pid == plugin_id))
            .and_then(|(_, _, desc)| desc.icon.clone())
    }

    /// Synchronizes plugin stream ownership after a non-plugin or plugin media open finishes.
    pub fn sync_media_open_state<P: PluginPlayer>(&mut self, player: &P) {
        self.stream.scoped_released = false;
        if !player.has_scoped_stream() {
            self.window.clear_active_stream();
            self.stream.stream_plugin = None;
            self.stream.clear_playlist();
        }
    }

    pub fn close_browse(&mut self, active_source: &mut ActiveSource) -> PluginBatchOutcome {
        self.browse.close_view(&self.window, active_source)
    }

    /// Cancels any in-flight plugin `resolve` request without disturbing directory browsing.
    pub fn cancel_resolve_load(&mut self) -> bool {
        let Some(pending) = self.browse.cancel_resolve() else {
            return false;
        };
        let _ = self
            .window
            .cancel_request(&pending.plugin_id, pending.req_id);
        true
    }

    pub fn cancel_requests_for(
        &mut self,
        active_source: &mut ActiveSource,
        pid: Option<&str>,
        sid: Option<&str>,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if self.browse.has_browse_for(pid, sid)
            || matches!(
                active_source,
                ActiveSource::Plugin(view)
                    if pid.is_none_or(|id| id == view.plugin_id)
                        && sid.is_none_or(|id| id == view.source_id)
            )
        {
            out.merge(self.close_browse(active_source));
        }
        if self.browse.has_media_load_for(pid, sid) && self.cancel_resolve_load() {
            out.effects.push(PluginSideEffect::MediaLoadCancelled);
        }
        out
    }

    pub fn reconcile_active_source(
        &mut self,
        active_source: &mut ActiveSource,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        let ActiveSource::Plugin(ps) = active_source else {
            return out;
        };
        let still_active = self
            .window
            .media_sources()
            .into_iter()
            .any(|(pid, _, src)| pid == ps.plugin_id && src.id == ps.source_id);
        if !still_active {
            let (pid, sid) = (ps.plugin_id.clone(), ps.source_id.clone());
            out.merge(self.cancel_requests_for(active_source, Some(&pid), Some(&sid)));
            *active_source = ActiveSource::Local;
            out.domain.catalog_changed = true;
        }
        out
    }

    /// Advances the media generation, releasing any active `ScopedStream`, dismissing stale Origin
    /// consent, and resetting timed overlays.
    pub fn advance_media<P: PluginPlayer>(&mut self, player: &mut P) -> (bool, PluginBatchOutcome) {
        self.annotations.cancel_in_flight();
        let connected = self.window.is_connected();
        let mut out = PluginBatchOutcome::default();
        match self.window.advance_media() {
            Ok(()) => {
                let had_scoped = player.has_scoped_stream();
                player.release_scoped();
                if had_scoped {
                    self.stream.scoped_released = true;
                }
                self.stream.stream_plugin = None;
                let has_modal = self.window.active_modal().is_some();
                if self
                    .consent
                    .clear_origin_on_media_advance(connected, has_modal, &mut self.modal)
                {
                    out.domain.consent_changed = true;
                }
                self.stream.clear_playlist();
                out.merge(self.reset_timed_overlays());
                (true, out)
            }
            Err(error) => {
                log::warn!("advance_plugin_media failed: {error:?}");
                out.push_toast(
                    format!("媒体切换暂未完成 · {}", error_code_label(error)),
                    true,
                );
                (false, out)
            }
        }
    }

    pub fn reset_timed_overlays(&mut self) -> PluginBatchOutcome {
        self.window.reset_timed_overlays();
        let had = self.overlay_frame.take().is_some();
        let mut out = PluginBatchOutcome::default();
        if had {
            out.domain.overlay_changed = true;
        }
        out.effects.push(PluginSideEffect::ClearAllGlyphs);
        out.effects.push(PluginSideEffect::ClearOverlayWake);
        out
    }

    pub fn sync_overlay_clock<P: PluginPlayer>(
        &mut self,
        player: &P,
        now: Instant,
    ) -> PluginBatchOutcome {
        let st = player.state();
        self.window.update_overlay_clock(
            st.time_pos,
            st.speed,
            st.paused || st.ended || !st.loaded,
            st.buffering,
            st.duration,
            now,
        );
        let mut out = PluginBatchOutcome::default();
        out.effects.push(PluginSideEffect::ClearOverlayWake);
        out
    }

    pub fn advance_seek<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if self.overlay_frame.take().is_some() {
            out.domain.overlay_changed = true;
        }
        out.effects.push(PluginSideEffect::ClearAllGlyphs);
        out.effects.push(PluginSideEffect::ClearOverlayWake);
        if let Some(report) = self.window.advance_overlay_seek() {
            out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        }
        out
    }

    pub fn reconcile_enhancements<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.reconcile_enhancements_ref(&mut ctx, now)
    }

    fn reconcile_enhancements_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let (media_loaded, position_ms) = {
            let st = ctx.player.state();
            (st.loaded, (st.time_pos.max(0.) * 1000.) as u64)
        };
        let (revoked, reports) = self
            .window
            .reconcile_enhancements(media_loaded, position_ms, now);
        let mut out = PluginBatchOutcome::default();
        let revoked_any = !revoked.revoked_owners.is_empty();
        for owner in revoked.revoked_owners {
            ctx.player.revoke_plugin_tracks(&owner);
            out.effects.push(PluginSideEffect::ClearOwnerGlyphs(owner));
        }
        if revoked.clear_timeline {
            ctx.player.flush_deferred();
            out.domain.timeline_changed = true;
        }
        if revoked.clear_lut {
            let _ = ctx.player.set_monitor_lut(None);
            out.domain.lut_changed = true;
        }
        for report in reports {
            out.merge(self.apply_poll_report_ref(ctx, report, now));
        }
        if revoked_any {
            out.domain.playback_changed = true;
            out.domain.overlay_changed = true;
            out.merge(self.step_timed_overlay_frame_ref(ctx, now));
        }
        out
    }

    pub fn update_overlay_window<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let media_loaded = ctx.player.state().loaded;
        let mut out = PluginBatchOutcome::default();
        for report in self.window.update_overlay_window(media_loaded, now) {
            out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        }
        out
    }

    pub fn step_timed_overlay_frame<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.step_timed_overlay_frame_ref(&mut ctx, now)
    }

    fn step_timed_overlay_frame_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let media_loaded = ctx.player.state().loaded;
        let (reports, mut out) =
            step_overlay_frame_inner(&mut self.window, media_loaded, &mut self.overlay_frame, now);
        for report in reports {
            out.merge(self.apply_poll_report_ref(ctx, report, now));
        }
        out
    }

    pub fn reconcile_lifetimes<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.reconcile_lifetimes_ref(&mut ctx, now)
    }

    fn reconcile_lifetimes_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = self.reconcile_enhancements_ref(ctx, now);
        if self.stream.reconcile_live(&self.window) {
            out.domain.stream_meta_cleared = true;
        }
        if self
            .browse
            .page_binding
            .as_ref()
            .is_some_and(|b| !b.is_live())
        {
            out.merge(self.close_browse(ctx.active_source));
            *ctx.active_source = ActiveSource::Local;
            out.domain.catalog_changed = true;
        }
        let has_modal = self.window.active_modal().is_some();
        let current_gen = self.window.generation();
        out.merge(self.consent.reconcile(
            &self.window,
            current_gen,
            has_modal,
            &mut self.modal,
            now,
        ));
        out
    }
}

#[cfg(test)]
mod tests;
