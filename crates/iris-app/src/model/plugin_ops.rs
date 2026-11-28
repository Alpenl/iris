//! Plugin poll/tick, command/widget/modal/consent dispatch, enhancement & overlay
//! lifecycle reconciliation, stream side-effect coordination, and thumbnail fetch
//! forwarding on [`super::AppModel`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use iris_api::v2::{ErrorCode, Id};
use iris_api::{PluginModal, SourceManageAction};
use iris_core::store::{AnnotationEdit, AnnotationSnapshot};
use iris_plugin_host::Event as PluginEvent;

use super::AppModel;
use crate::playback::{NowPlayingSnapshot, PlaybackSessionPort};
use crate::plugins::{
    OpenUrlOutcome, PendingHostAnnotation, PluginBatchOutcome, PluginDomainChanges, PluginPlayer,
    PluginSideEffect,
};
use crate::workbench::WorkbenchDomainChanges;

impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    pub fn reconcile_plugin_domain(&mut self, domain: &mut PluginDomainChanges) {
        if domain.locations_reloaded {
            self.catalog.reload_locations();
        }
        if domain.uninstalled_changed && self.reconcile_missing_plugin_theme() {
            domain.theme_changed = true;
        }
    }

    pub fn poll_plugins(&mut self, now: Instant) -> (PluginBatchOutcome, bool) {
        let mut out = self.with_plugin_ctx(|ctrl, ctx| ctrl.poll(ctx, now));
        let prev_theme = out.domain.theme_changed;
        self.reconcile_plugin_domain(&mut out.domain);
        let theme_fallback = !prev_theme && out.domain.theme_changed;
        if out.domain.catalog_changed || out.domain.favorites_changed {
            self.rebuild_directory_rows();
        }
        (out, theme_fallback)
    }

    pub fn maybe_tick_plugins(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.maybe_tick(ctx, now))
    }

    pub fn broadcast_plugin_state_changed(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.broadcast_state_changed(ctx, now))
    }

    pub fn broadcast_plugin_event(
        &mut self,
        event: PluginEvent,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.broadcast(ctx, event, now))
    }

    pub fn announce_plugin_media_if_loaded(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.announce_media_if_loaded(ctx, now))
    }

    pub fn reconcile_plugin_lifetimes(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.reconcile_lifetimes(ctx, now))
    }

    pub fn reconcile_plugin_enhancements(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.reconcile_enhancements(ctx, now))
    }

    pub fn sync_overlay_clock(&mut self, now: Instant) -> PluginBatchOutcome {
        self.plugins
            .sync_overlay_clock(self.playback.session(), now)
    }

    pub fn advance_plugin_seek(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.advance_seek(ctx, now))
    }

    pub fn update_overlay_window(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.update_overlay_window(ctx, now))
    }

    pub fn step_timed_overlay_frame(&mut self, now: Instant) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.step_timed_overlay_frame(ctx, now))
    }

    pub fn advance_plugin_media(&mut self) -> (bool, PluginBatchOutcome) {
        self.plugins.advance_media(self.playback.session_mut())
    }

    /// Applies playback/catalog state transitions implied by a single [`PluginSideEffect`],
    /// returning `(loading_generation_to_schedule, now_playing_update)`.
    pub fn apply_plugin_media_side_effect(
        &mut self,
        effect: &PluginSideEffect,
        now: Instant,
    ) -> (Option<u64>, Option<Option<NowPlayingSnapshot>>) {
        match effect {
            PluginSideEffect::MediaResolveStarted => {
                self.catalog.cancel_remote_load();
                let generation = self.playback.begin_plugin_resolve();
                (Some(generation), None)
            }
            PluginSideEffect::MediaLoadCancelled => {
                let _ = self.playback.cancel_in_flight_load(true);
                (None, None)
            }
            PluginSideEffect::MediaOpenSucceeded => {
                self.catalog.cancel_remote_load();
                self.playback.commit_media_open_generation();
                self.finish_media_open();
                (None, None)
            }
            PluginSideEffect::MediaOpenFailed { .. } => {
                self.catalog.cancel_remote_load();
                self.playback.commit_media_open_generation();
                let np = self.playback.on_media_open_failed(now);
                (None, np)
            }
            _ => (None, None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_plugin_entry(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        entry_id: &str,
        title: Option<String>,
        variant: Option<String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.resolve_entry(ctx, plugin_id, source_id, entry_id, title, variant, now)
        })
    }

    pub fn switch_stream_variant(
        &mut self,
        variant_id: &str,
        now: Instant,
    ) -> Option<PluginBatchOutcome> {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.switch_stream_variant(ctx, variant_id, now))
    }

    pub fn open_user_url(
        &mut self,
        url: &str,
        selected: &str,
        now: Instant,
    ) -> (OpenUrlOutcome, PluginBatchOutcome) {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.open_user_url(ctx, url, selected, now))
    }

    pub fn on_source_request_timeout(
        &mut self,
        plugin_id: &str,
        request_id: Id,
    ) -> PluginBatchOutcome {
        let plugins = &mut self.plugins;
        self.catalog.with_plugin_parts(|_, active_source| {
            plugins.on_source_request_timeout(active_source, plugin_id, request_id)
        })
    }

    pub fn invoke_plugin_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.invoke_command(ctx, plugin_id, command_id, now))
    }

    pub fn invoke_plugin_widget_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        value: Option<String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.invoke_widget_action(ctx, plugin_id, action_id, value, now)
        })
    }

    pub fn fire_view_timer_tick(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.fire_view_timer_tick(ctx, plugin_id, action_id, now))
    }

    pub fn submit_active_plugin_modal(
        &mut self,
        collect_values: impl FnOnce(&str, &PluginModal) -> BTreeMap<String, String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.submit_active_modal(ctx, collect_values, now))
    }

    pub fn cancel_active_plugin_modal(&mut self, now: Instant) -> Option<PluginBatchOutcome> {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.cancel_active_modal(ctx, now))
    }

    pub fn clear_plugin_modal_feedback(&mut self) {
        self.plugins.modal_mut().clear_feedback();
    }

    pub fn take_plugin_modal_focus_pending(&mut self) -> bool {
        self.plugins.modal_mut().take_focus_pending()
    }

    pub fn mark_plugin_modal_focus_pending(&mut self) {
        self.plugins.modal_mut().mark_focus_pending();
    }

    pub fn review_plugin_package(&mut self, path: PathBuf) -> PluginBatchOutcome {
        self.plugins.review_package_path(path)
    }

    pub fn approve_host_consent(&mut self) -> PluginBatchOutcome {
        self.plugins.approve_host_consent()
    }

    pub fn cancel_host_consent(&mut self) -> PluginBatchOutcome {
        self.plugins.cancel_host_consent()
    }

    pub fn toggle_insecure_auth(&mut self, origin_id: Id) -> bool {
        self.plugins.toggle_insecure_auth(origin_id)
    }

    pub fn toggle_keep_identity(&mut self, review_id: Id) -> bool {
        self.plugins.toggle_keep_identity(review_id)
    }

    pub fn toggle_plugin_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        now: Instant,
    ) -> (PluginBatchOutcome, WorkbenchDomainChanges) {
        self.workbench.set_plugin_enabled_preference(id, enabled);
        let out =
            self.with_plugin_ctx(|ctrl, ctx| ctrl.toggle_plugin_enabled(ctx, id, enabled, now));
        let mut wb = WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        };
        if out.dirty_slots.theme || out.domain.theme_changed {
            wb.theme_changed = true;
        }
        (out, wb)
    }

    pub fn uninstall_plugin(
        &mut self,
        id: &str,
        keep_data: bool,
        now: Instant,
    ) -> (bool, PluginBatchOutcome, WorkbenchDomainChanges) {
        let (rejected, mut out) =
            self.with_plugin_ctx(|ctrl, ctx| ctrl.uninstall_plugin(ctx, id, keep_data, now));
        if rejected {
            return (true, out, WorkbenchDomainChanges::empty());
        }
        let wb = self.workbench.on_plugin_uninstalled(id);
        if wb.theme_changed {
            out.domain.theme_changed = true;
        }
        (false, out, wb)
    }

    pub fn reload_plugins(&mut self, now: Instant) -> (Vec<String>, PluginBatchOutcome) {
        let disabled = self.workbench.settings.disabled_plugins.clone();
        self.with_plugin_ctx(|ctrl, ctx| ctrl.reload_plugins(ctx, &disabled, now))
    }

    pub fn manage_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        action: SourceManageAction,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.with_plugin_ctx(|ctrl, ctx| ctrl.manage_source(ctx, plugin_id, source_id, action, now))
    }

    pub fn begin_host_annotation(
        &mut self,
        edit: Option<AnnotationEdit>,
    ) -> Result<Option<PendingHostAnnotation>, PluginBatchOutcome> {
        self.catalog
            .begin_host_annotation(&mut self.plugins, self.playback.session(), edit)
    }

    pub fn finish_host_annotation(
        &mut self,
        saving: bool,
        result: Result<AnnotationSnapshot, ErrorCode>,
    ) -> (bool, PluginBatchOutcome) {
        self.catalog
            .finish_host_annotation(&mut self.plugins, saving, result)
    }
}
