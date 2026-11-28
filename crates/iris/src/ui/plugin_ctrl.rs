//! Thin GPUI bridge translating [`iris_app::PluginBatchOutcome`] domain changes and
//! side effects into regional `DirtyRegions` notifications, modal focus sync, and
//! background timer tasks.

use std::time::{Duration, Instant};

use gpui::{Context, Window};
use iris_api::v2::Id;
use iris_app::{PluginBatchOutcome, PluginSideEffect};

use super::WindowShell;
use super::views::{DirtyRegions, dirty_regions_for_plugin_outcome};

impl WindowShell {
    pub(super) fn has_active_modal(&self) -> bool {
        self.has_host_consent()
            || self.overlays.settings_open
            || self.overlays.palette_open
            || self.overlays.dialog.is_some()
            || self.plugins().active_modal().is_some()
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
            self.settings().directory_open
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
                let out = this.app.on_source_request_timeout(&plugin_id, request_id);
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
        let (outcome, _) = self.app.poll_plugins(Instant::now());
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
        let mut d = outcome.domain;
        self.app.reconcile_plugin_domain(&mut d);

        let mut regions = dirty_regions_for_plugin_outcome(
            outcome.dirty_slots,
            outcome.status_changed,
            &d,
            self.ui_visibility(),
        );

        if d.close_overlays {
            self.overlays.close_all();
        }
        if d.stream_meta_cleared {
            self.overlays.close_variants_popover();
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

        let now = Instant::now();
        for effect in outcome.effects {
            let (loading_gen, np_update) = self.app.apply_plugin_media_side_effect(&effect, now);
            if let Some(generation) = loading_gen {
                self.schedule_loading_indicator(generation, cx);
            }
            if let Some(np) = np_update {
                self.apply_now_playing_snapshot(np);
            }
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
                    self.catalog_ui.remote_load = None;
                    regions.root = true;
                    regions.viewport = true;
                    regions.dock = true;
                }
                PluginSideEffect::MediaLoadCancelled => {
                    self.plugin_ui.resolve_timeout = None;
                    regions.viewport = true;
                }
                PluginSideEffect::MediaOpenSucceeded => {
                    self.plugin_ui.resolve_timeout = None;
                    self.catalog_ui.remote_load = None;
                    self.overlays.close_menus();
                    self.active_mut().image.reset();
                    self.on_media_opened_ui(cx);
                    regions = DirtyRegions::ALL;
                }
                PluginSideEffect::MediaOpenFailed { .. } => {
                    self.plugin_ui.resolve_timeout = None;
                    self.catalog_ui.remote_load = None;
                    self.overlays.close_menus();
                    self.active_mut().image.reset();
                    self.sync_viewport_effects(cx);
                    regions = DirtyRegions::ALL;
                }
                PluginSideEffect::PlaybackStopped => {
                    self.active_mut().image.reset();
                    self.sleep_preventer
                        .set_active(self.app.playback().should_prevent_sleep());
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

        if self.plugins().modal().focus_pending()
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
        if !self.app.take_plugin_modal_focus_pending() {
            return;
        }
        let modal = self.app.plugins().active_modal();
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
        let out = self.app.submit_active_plugin_modal(
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
        let Some(out) = self.app.cancel_active_plugin_modal(Instant::now()) else {
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
        let out = self
            .app
            .invoke_plugin_command(plugin_id, command_id, Instant::now());
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
        let out = self
            .app
            .invoke_plugin_widget_action(plugin_id, action_id, value, Instant::now());
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
        if !enabled {
            self.text_inputs.clear_plugin_sidebar_fields(id);
        }
        let (out, wb) = self.app.toggle_plugin_enabled(id, enabled, Instant::now());
        let plugin_regions = self.apply_plugin_outcome(out, None, cx);
        let wb_regions = self.apply_workbench_changes(wb, None, cx);
        self.notify_regions(
            plugin_regions.union(wb_regions).union(DirtyRegions::ALL),
            cx,
        );
    }

    pub(super) fn uninstall_plugin(&mut self, id: &str, keep_data: bool, cx: &mut Context<Self>) {
        let (rejected, out, wb) = self.app.uninstall_plugin(id, keep_data, Instant::now());
        if rejected {
            let _ = self.apply_plugin_outcome(out, None, cx);
            return;
        }
        self.text_inputs.clear_plugin_sidebar_fields(id);
        let plugin_regions = self.apply_plugin_outcome(out, None, cx);
        let wb_regions = self.apply_workbench_changes(wb, None, cx);
        self.notify_regions(
            plugin_regions.union(wb_regions).union(DirtyRegions::ALL),
            cx,
        );
    }

    pub(super) fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        let (enabled_ids, out) = self.app.reload_plugins(Instant::now());
        let _ = self.apply_plugin_outcome(out, None, cx);
        self.text_inputs
            .retain_enabled_sidebar_fields(|pid| enabled_ids.iter().any(|id| id == pid));
        self.sync_viewport_effects(cx);
        self.notify_all(cx);
    }
}
