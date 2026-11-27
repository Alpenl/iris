//! Consent/modal submission, host annotations, command/widget/timer dispatch,
//! and plugin lifecycle management (`enable`, `uninstall`, `reload`, `manage_source`,
//! exit commands, and theme reset) on [`super::PluginController`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use iris_api::v2::{Capability, ErrorCode, HostEvent};
use iris_api::{PluginModal, SourceManageAction};
use iris_core::settings::Settings;
use iris_core::store::{AnnotationEdit as StoreAnnotationEdit, AnnotationSnapshot, Store};
use iris_plugin_host::PluginStatus;

use super::PluginController;
use super::consent::HostConsentState;
use super::enhance::PendingHostAnnotation;
use super::ports::{PluginBatchOutcome, PluginContext, PluginPlayer};
use super::sources::ActiveSource;

impl PluginController {
    pub fn review_package_path(&mut self, path: PathBuf) -> PluginBatchOutcome {
        HostConsentState::review_package(&self.window, path)
    }

    pub fn approve_host_consent(&mut self) -> PluginBatchOutcome {
        self.consent.approve_front(&self.window)
    }

    pub fn cancel_host_consent(&mut self) -> PluginBatchOutcome {
        let has_modal = self.window.active_modal().is_some();
        self.consent
            .cancel_front(&self.window, has_modal, &mut self.modal)
    }

    pub fn submit_active_modal<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        collect_values: impl FnOnce(&str, &PluginModal) -> BTreeMap<String, String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if self.has_host_consent() {
            return out;
        }
        let Some((pid, modal)) = self
            .window
            .active_modal()
            .map(|(p, m)| (p.to_owned(), m.clone()))
        else {
            return out;
        };
        let raw_values = collect_values(&pid, &modal);
        let Some(values) = self.modal.begin_submit(&pid, &modal, raw_values) else {
            return out;
        };
        out.domain.modal_changed = true;
        let report = self.window.send_to(
            &pid,
            HostEvent::ModalSubmit {
                id: modal.id,
                values,
            },
        );
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out
    }

    pub fn cancel_active_modal<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> Option<PluginBatchOutcome> {
        let (pid, modal_id) = self
            .window
            .active_modal()
            .map(|(p, m)| (p.to_owned(), m.id.clone()))?;
        self.window.dismiss_modal(&pid);
        let report = self
            .window
            .send_to(&pid, HostEvent::ModalCancel { id: modal_id });
        self.modal.on_cancel();
        let mut out = self.apply_poll_report_ref(&mut ctx, report, now);
        out.domain.modal_changed = true;
        Some(out)
    }

    pub fn grant_current_annotations<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        plugin_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.grant_current_annotations_ref(&mut ctx, plugin_id, now)
    }

    pub(super) fn grant_current_annotations_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        plugin_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        let Some(uri) = ctx
            .player
            .may_share_current_identity()
            .then(|| ctx.player.current_media_uri())
            .flatten()
        else {
            return PluginBatchOutcome::default();
        };
        let Some(report) = self.window.grant_annotations(plugin_id, vec![uri]) else {
            return PluginBatchOutcome::default();
        };
        self.apply_poll_report_ref(ctx, report, now)
    }

    pub fn begin_host_annotation<P: PluginPlayer>(
        &mut self,
        player: &P,
        store: &Store,
        edit: Option<StoreAnnotationEdit>,
    ) -> Result<Option<PendingHostAnnotation>, PluginBatchOutcome> {
        self.annotations.begin(&self.host, player, store, edit)
    }

    pub fn finish_host_annotation(
        &mut self,
        store: &mut Store,
        saving: bool,
        result: Result<AnnotationSnapshot, ErrorCode>,
    ) -> (bool, PluginBatchOutcome) {
        self.annotations.finish(store, saving, result)
    }

    pub fn invoke_command<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        plugin_id: &str,
        command_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.invoke_command_ref(&mut ctx, plugin_id, command_id, now)
    }

    pub(super) fn invoke_command_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        plugin_id: &str,
        command_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = self.grant_current_annotations_ref(ctx, plugin_id, now);
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let report = self.window.send_to(
            plugin_id,
            HostEvent::Command {
                id: command_id.to_owned(),
                snapshot: Some(snap),
            },
        );
        out.merge(self.apply_poll_report_ref(ctx, report, now));
        out
    }

    pub fn invoke_widget_action<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        plugin_id: &str,
        action_id: &str,
        value: Option<String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = self.grant_current_annotations_ref(&mut ctx, plugin_id, now);
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let report = self.window.send_to(
            plugin_id,
            HostEvent::Action {
                id: action_id.to_owned(),
                value,
                snapshot: Some(snap),
            },
        );
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out
    }

    pub fn fire_view_timer_tick<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        plugin_id: &str,
        action_id: &str,
        now: Instant,
    ) -> PluginBatchOutcome {
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let report = self.window.send_to(
            plugin_id,
            HostEvent::Action {
                id: action_id.to_owned(),
                value: None,
                snapshot: Some(snap),
            },
        );
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    pub fn queue_deferred_commands<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        commands: Vec<(String, String)>,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.deferred.extend(commands);
        self.run_deferred_commands_ref(&mut ctx, now)
    }

    pub(super) fn run_deferred_commands_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        let pending = std::mem::take(&mut self.deferred);
        for (plugin, command) in pending {
            if self.window.is_ready(&plugin) {
                out.merge(self.invoke_command_ref(ctx, &plugin, &command, now));
            } else if self
                .window
                .list()
                .iter()
                .find(|p| p.id == plugin)
                .is_none_or(|p| p.enabled && !matches!(p.status, PluginStatus::Failed(_)))
            {
                self.deferred.push((plugin, command));
            }
        }
        out
    }

    pub fn toggle_plugin_enabled<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        id: &str,
        enabled: bool,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if !enabled {
            out.merge(self.cleanup_playback_ref(&mut ctx, Some(id), None));
        }
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let report = self.window.set_enabled_with_snapshot(id, enabled, snap);
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out.merge(self.reconcile_active_source(ctx.active_source));
        out
    }

    pub fn uninstall_plugin<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        id: &str,
        keep_data: bool,
        now: Instant,
    ) -> (bool, PluginBatchOutcome) {
        let mut out = PluginBatchOutcome::default();
        if id.to_ascii_lowercase().starts_with("iris.") {
            out.push_toast("官方内置插件不可删除", false);
            return (true, out);
        }
        out.merge(self.cleanup_playback_ref(&mut ctx, Some(id), None));
        self.window.dismiss_modal(id);
        let report = self.window.uninstall(id, keep_data);
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out.merge(self.reconcile_active_source(ctx.active_source));
        (false, out)
    }

    pub fn reload_plugins<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        disabled_plugins: &[String],
        now: Instant,
    ) -> (Vec<String>, PluginBatchOutcome) {
        let mut out = self.cleanup_playback_ref(&mut ctx, None, None);
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let report = self.window.reload_with_snapshot(disabled_plugins, snap);
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        let enabled_ids: Vec<String> = self
            .window
            .list()
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.id.clone())
            .collect();
        out.merge(self.reconcile_active_source(ctx.active_source));
        out.push_toast("插件列表已重新扫描", false);
        (enabled_ids, out)
    }

    pub fn manage_source<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        plugin_id: &str,
        source_id: &str,
        action: SourceManageAction,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if action == SourceManageAction::Remove {
            out.merge(self.cleanup_playback_ref(&mut ctx, Some(plugin_id), Some(source_id)));
            if matches!(
                &*ctx.active_source,
                ActiveSource::Plugin(ps) if ps.plugin_id == plugin_id && ps.source_id == source_id
            ) {
                *ctx.active_source = ActiveSource::Local;
                out.domain.catalog_changed = true;
            }
        }
        let report = self.window.send_to(
            plugin_id,
            HostEvent::SourceManage {
                source_id: source_id.to_owned(),
                action,
            },
        );
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out
    }

    pub fn send_exit_commands<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        commands: Vec<(String, String)>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        for (pid, cmd_id) in commands {
            let snap = self.playback_snapshot(ctx.player, ctx.store);
            let report = self.window.send_to(
                &pid,
                HostEvent::Command {
                    id: cmd_id,
                    snapshot: Some(snap),
                },
            );
            out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        }
        out
    }

    /// Disables all active `UiTheme` plugins, dismisses all plugin modals, and updates
    /// `settings.disabled_plugins` during `ResetAppearance`.
    pub fn reset_theme_plugins_and_modals<P: PluginPlayer>(
        &mut self,
        player: &mut P,
        store: &mut Store,
        settings: &mut Settings,
        active_source: &mut ActiveSource,
        now: Instant,
    ) -> (Vec<String>, PluginBatchOutcome) {
        let mut out = PluginBatchOutcome::default();
        let theme_plugin_ids: Vec<String> = self
            .window
            .list()
            .iter()
            .filter(|p| {
                p.enabled
                    && p.binding
                        .as_ref()
                        .is_some_and(|b| b.installed.grants.contains(&Capability::UiTheme))
            })
            .map(|p| p.id.clone())
            .collect();
        for pid in &theme_plugin_ids {
            if !settings.disabled_plugins.iter().any(|d| d == pid) {
                settings.disabled_plugins.push(pid.clone());
            }
            {
                let mut ctx = PluginContext {
                    player,
                    store,
                    settings,
                    active_source,
                };
                out.merge(self.cleanup_playback_ref(&mut ctx, Some(pid), None));
                let snap = self.playback_snapshot(ctx.player, ctx.store);
                let report = self.window.set_enabled_with_snapshot(pid, false, snap);
                out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
            }
        }
        while let Some((plugin, _)) = self.window.active_modal() {
            let plugin = plugin.to_owned();
            self.window.dismiss_modal(&plugin);
        }
        out.merge(self.reconcile_active_source(active_source));
        self.modal.mark_focus_pending();
        out.domain.modal_changed = true;
        out.domain.theme_changed = true;
        (theme_plugin_ids, out)
    }
}
