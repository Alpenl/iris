//! Thin GPUI bridge translating [`iris_app::PluginBatchOutcome`] domain changes and
//! side effects into regional `DirtyRegions` notifications, modal focus sync, and
//! background timer tasks.

use std::time::{Duration, Instant};

use gpui::{Context, Window};
use iris_api::v2::Id;
use iris_app::{PluginBatchOutcome, PluginContext, PluginController, PluginSideEffect};
use iris_core::player::Session;

use super::AppModel;
use super::views::{DirtyRegions, dirty_regions_for_plugin_outcome};

impl AppModel {
    pub(in crate::ui) fn with_plugin_ctx<R>(
        &mut self,
        f: impl FnOnce(&mut PluginController, PluginContext<'_, Session>) -> R,
    ) -> R {
        let ctx = PluginContext {
            player: self.playback.session_mut(),
            store: &mut self.store,
            settings: &self.settings,
            active_source: &mut self.catalog.active_source,
        };
        f(&mut self.plugins, ctx)
    }

    pub(super) fn has_active_modal(&self) -> bool {
        self.has_host_consent()
            || self.overlays.settings_open
            || self.overlays.palette_open
            || self.overlays.dialog.is_some()
            || self.plugins.active_modal().is_some()
    }

    /// Single source of truth for any overlay that blocks background playback / workspace shortcuts.
    pub(super) fn has_blocking_overlay(&self) -> bool {
        self.has_active_modal()
            || self.overlays.context_menu.is_some()
            || self.overlays.popover.is_some()
    }

    pub(super) fn sidebar_is_visible(&self) -> bool {
        if self.chrome.fullscreen {
            self.overlays.dir_popup.is_open()
        } else {
            self.settings.directory_open
        }
    }

    fn spawn_plugin_source_timeout(
        &self,
        plugin_id: String,
        request_id: Id,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        let timer = cx.background_executor().timer(timeout);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                let out = this.plugins.on_source_request_timeout(
                    &mut this.catalog.active_source,
                    &plugin_id,
                    request_id,
                );
                let regions = this.apply_plugin_outcome(out, None, cx);
                if regions.any() {
                    this.notify_regions(regions, cx);
                }
            });
        })
    }

    /// Drains background plugin messages (`cx.spawn_in` wakeup), auto-focusing
    /// newly opened modals and notifying only the regional GPUI entities marked dirty.
    pub(super) fn poll_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let outcome = self.with_plugin_ctx(|ctrl, ctx| ctrl.poll(ctx, Instant::now()));
        let regions = self.apply_plugin_outcome(outcome, Some(window), cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    /// Applies [`PluginBatchOutcome`]: maps pure domain changes to regional `DirtyRegions`,
    /// dispatches platform/GPUI side effects in deterministic order, and syncs modal focus & timers.
    pub(in crate::ui) fn apply_plugin_outcome(
        &mut self,
        outcome: PluginBatchOutcome,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        let mut regions = dirty_regions_for_plugin_outcome(
            outcome.dirty_slots,
            outcome.status_changed,
            &outcome.domain,
            self.ui_visibility(),
        );

        let d = outcome.domain;
        if d.locations_reloaded {
            self.locations.reload();
        }
        if d.close_overlays {
            self.overlays.close_all();
        }
        if d.stream_meta_cleared {
            self.overlays.close_variants_popover();
        }
        if d.uninstalled_changed
            && let Some(pid) = self.settings.theme_id.strip_prefix("plugin:")
            && !self.plugins.list().iter().any(|p| p.id == pid)
        {
            self.settings.theme_id = "graphite".into();
            self.persist_user_settings();
        }
        if d.theme_changed {
            self.sync_theme_tokens(cx);
            self.sync_viewport_effects(cx);
        }
        if d.favorites_changed {
            self.rebuild_directory_rows();
            self.scan_visible_favorites(cx);
        } else if d.catalog_changed {
            self.rebuild_directory_rows();
            self.ensure_filmstrip_thumbnails(cx);
        }

        for effect in outcome.effects {
            match effect {
                PluginSideEffect::ShowToast { message, is_error } => {
                    if is_error {
                        self.show_error_toast(message, cx);
                    } else {
                        self.show_toast(message, cx);
                    }
                    regions.root = true;
                }
                PluginSideEffect::StepMedia { delta, wrap } => {
                    self.step(delta, wrap, cx);
                    regions = DirtyRegions::ALL;
                }
                PluginSideEffect::RefreshSource {
                    plugin_id,
                    source_id,
                } => {
                    self.refresh_plugin_source(&plugin_id, &source_id, cx);
                    regions.sidebar = true;
                }
                PluginSideEffect::OpenExternalUrl(url) => {
                    cx.open_url(&url);
                }
                PluginSideEffect::ScheduleBrowseTimeout {
                    plugin_id,
                    request_id,
                    timeout,
                } => {
                    self.plugin_ui.browse_timeout =
                        Some(self.spawn_plugin_source_timeout(plugin_id, request_id, timeout, cx));
                }
                PluginSideEffect::ScheduleResolveTimeout {
                    plugin_id,
                    request_id,
                    timeout,
                } => {
                    self.plugin_ui.resolve_timeout =
                        Some(self.spawn_plugin_source_timeout(plugin_id, request_id, timeout, cx));
                }
                PluginSideEffect::MediaResolveStarted => {
                    let _ = self.catalog.remote_load.take();
                    self.playback.begin_plugin_resolve();
                    regions.root = true;
                    regions.viewport = true;
                    regions.dock = true;
                }
                PluginSideEffect::MediaLoadCancelled => {
                    self.plugin_ui.resolve_timeout = None;
                    let _ = self.playback.cancel_in_flight_load(true);
                    regions.viewport = true;
                }
                PluginSideEffect::MediaOpenSucceeded => {
                    self.plugin_ui.resolve_timeout = None;
                    let _ = self.catalog.remote_load.take();
                    self.overlays.close_menus();
                    self.playback.commit_media_open_generation();
                    self.active_mut().image.reset();
                    self.finish_media_open(cx);
                    regions = DirtyRegions::ALL;
                }
                PluginSideEffect::MediaOpenFailed { .. } => {
                    self.plugin_ui.resolve_timeout = None;
                    let _ = self.catalog.remote_load.take();
                    self.overlays.close_menus();
                    self.playback.commit_media_open_generation();
                    if let Some(np) = self.playback.on_media_open_failed(Instant::now()) {
                        self.apply_now_playing_snapshot(np);
                    }
                    self.active_mut().image.reset();
                    self.sync_viewport_effects(cx);
                    regions = DirtyRegions::ALL;
                }
                PluginSideEffect::PlaybackStopped => {
                    self.active_mut().image.reset();
                    self.sleep_preventer
                        .set_active(self.playback.should_prevent_sleep());
                    self.sync_now_playing(true);
                    regions.viewport = true;
                    regions.dock = true;
                }
                PluginSideEffect::ScheduleOverlayWake { delay } => {
                    if self.plugin_ui.overlay_wake.is_none() {
                        let timer = cx.background_executor().timer(delay);
                        self.plugin_ui.overlay_wake = Some(cx.spawn(async move |this, cx| {
                            timer.await;
                            let _ = this.update(cx, |this, cx| {
                                this.plugin_ui.overlay_wake = None;
                                this.step_timed_overlay_frame(cx);
                            });
                        }));
                    }
                }
                PluginSideEffect::ClearOverlayWake => {
                    self.plugin_ui.overlay_wake = None;
                }
                PluginSideEffect::ClearOwnerGlyphs(owner) => {
                    self.plugin_ui.glyphs.clear_owner(&owner);
                }
                PluginSideEffect::ClearAllGlyphs => {
                    self.plugin_ui.glyphs.clear();
                }
                PluginSideEffect::ResetThumbnails => {
                    self.plugin_ui.thumbnails.reset();
                }
            }
        }

        if self.plugins.modal().focus_pending()
            && let Some(window) = window
        {
            self.sync_plugin_modal_focus(window);
        }
        self.sync_view_timer(cx);
        regions
    }

    pub(super) fn sync_plugin_modal_focus(&mut self, window: &mut Window) {
        if self.has_host_consent() {
            self.text_inputs.active_form_key = None;
            if !self.focus.is_focused(window) {
                self.focus.focus(window);
            }
            return;
        }
        if !self.plugins.modal_mut().take_focus_pending() {
            return;
        }
        let modal = self.plugins.active_modal();
        if modal.is_some() {
            self.overlays.close_all();
        }
        let focus_input = self.text_inputs.sync_plugin_modal(modal);
        let current_modal_input = modal.is_some()
            && self
                .text_inputs
                .active_form_key
                .as_deref()
                .is_some_and(|key| key.starts_with("modal:"))
            && self.text_inputs.form_focus.is_focused(window);
        if focus_input {
            self.text_inputs.form_focus.focus(window);
        } else if !current_modal_input {
            self.focus.focus(window);
        }
    }

    /// Sends `ModalSubmit` while keeping the modal open and preserving user-entered
    /// `modal:*` fields until the plugin validates and replies `CloseModal`.
    pub(super) fn submit_active_plugin_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text_inputs = &self.text_inputs;
        let ctx = PluginContext {
            player: self.playback.session_mut(),
            store: &mut self.store,
            settings: &self.settings,
            active_source: &mut self.catalog.active_source,
        };
        let out = self.plugins.submit_active_modal(
            ctx,
            |pid, modal| text_inputs.collect_modal_values(pid, modal),
            Instant::now(),
        );
        let regions = self.apply_plugin_outcome(out, Some(window), cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    /// Unconditionally dismisses the active plugin modal (`Esc` / Cancel / scrim click),
    /// clears `modal:*` fields, and notifies the plugin via `ModalCancel`.
    pub(super) fn cancel_active_plugin_modal(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(out) =
            self.with_plugin_ctx(|ctrl, ctx| ctrl.cancel_active_modal(ctx, Instant::now()))
        else {
            return;
        };
        self.text_inputs.clear_modal_fields();
        let regions = self
            .apply_plugin_outcome(out, window, cx)
            .union(DirtyRegions::ALL);
        self.bump_activity(cx);
        self.notify_regions(regions, cx);
    }

    pub(super) fn invoke_plugin_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.invoke_command(ctx, plugin_id, command_id, Instant::now())
        });
        let regions = self.apply_plugin_outcome(out, Some(window), cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn invoke_plugin_widget_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        value: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.invoke_widget_action(ctx, plugin_id, action_id, value, Instant::now())
        });
        let regions = self.apply_plugin_outcome(out, Some(window), cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn invoke_plugin_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invoke_plugin_widget_action(plugin_id, action_id, None, window, cx);
    }

    pub(super) fn toggle_plugin_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.preset_rt
            .set_plugin_enabled(&mut self.settings, id, enabled);
        if !enabled {
            self.text_inputs.clear_plugin_sidebar_fields(id);
        }
        let out = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.toggle_plugin_enabled(ctx, id, enabled, Instant::now())
        });
        let _ = self.apply_plugin_outcome(out, None, cx);
        self.sync_viewport_effects(cx);
        self.notify_all(cx);
    }

    pub(super) fn uninstall_plugin(&mut self, id: &str, keep_data: bool, cx: &mut Context<Self>) {
        if id.to_ascii_lowercase().starts_with("iris.") {
            self.show_toast("官方内置插件不可删除", cx);
            return;
        }
        self.preset_rt
            .set_plugin_enabled(&mut self.settings, id, true);
        self.text_inputs.clear_plugin_sidebar_fields(id);
        if self.settings.theme_id == format!("plugin:{id}") {
            self.settings.theme_id = "graphite".into();
            self.persist_user_settings();
            self.sync_theme_tokens(cx);
        }
        let (_, out) = self
            .with_plugin_ctx(|ctrl, ctx| ctrl.uninstall_plugin(ctx, id, keep_data, Instant::now()));
        let _ = self.apply_plugin_outcome(out, None, cx);
        self.sync_viewport_effects(cx);
        self.notify_all(cx);
    }

    pub(super) fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        let disabled = self.settings.disabled_plugins.clone();
        let (enabled_ids, out) =
            self.with_plugin_ctx(|ctrl, ctx| ctrl.reload_plugins(ctx, &disabled, Instant::now()));
        let _ = self.apply_plugin_outcome(out, None, cx);
        self.text_inputs
            .retain_enabled_sidebar_fields(|pid| enabled_ids.iter().any(|id| id == pid));
        self.sync_viewport_effects(cx);
        self.notify_all(cx);
    }
}
