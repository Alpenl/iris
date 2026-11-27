//! Plugin `PollReport` application, `DirtySlots` UI redraw gating, modal async
//! validation & focus synchronization, and `HostCommand` execution.

use gpui::{Context, Window};
use iris_api::v2::{HostCommand, HostEvent, source::PlaybackSnapshot};
use iris_core::media_uri::MediaUri;
use iris_core::player::Command;
use iris_core::preset::SidebarMode;
use iris_plugin_host::ui::{self as plugin, PollReport};

use super::IrisWindow;
use super::state::{ActiveSource, PopoverKind, SourceErrorKind};

impl IrisWindow {
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

    pub(super) fn reconcile_active_source(&mut self) {
        let ActiveSource::Plugin(ps) = &self.catalog.active_source else {
            return;
        };
        let still_active = self
            .plugins
            .media_sources()
            .into_iter()
            .any(|(pid, _, src)| pid == ps.plugin_id && src.id == ps.source_id);
        if !still_active {
            let (pid, sid) = (ps.plugin_id.clone(), ps.source_id.clone());
            self.cancel_plugin_requests(Some(&pid), Some(&sid));
            self.catalog.active_source = ActiveSource::Local;
            self.rebuild_directory_rows();
        }
    }

    pub(super) fn plugin_snapshot(&self) -> PlaybackSnapshot {
        let session = &self.active().session;
        let fav = session
            .current_media_uri()
            .is_some_and(|u| self.store.is_favorite(&u));
        plugin::snapshot_from_session(session, fav, self.plugin_media_id())
    }

    pub(super) fn sidebar_is_visible(&self) -> bool {
        if self.chrome.fullscreen {
            self.overlays.dir_popup.is_open()
        } else {
            self.settings.directory_open
        }
    }

    /// Drains background plugin messages (`cx.spawn_in` wakeup), auto-focusing
    /// newly opened modals and only notifying GPUI when `PollReport::should_notify_ui` holds.
    pub(super) fn poll_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let report = self.plugins.poll();
        self.bind_plugin_current_file();
        self.reconcile_plugin_lifetimes(cx);
        if self.apply_poll_report(report, Some(window), cx) {
            cx.notify();
        }
        self.run_deferred_plugin_commands(window, cx);
    }

    pub(super) fn broadcast_plugin_event(
        &mut self,
        event: plugin::Event,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.bind_plugin_current_file();
        let report = self.plugins.broadcast(&event);
        self.apply_poll_report(report, window, cx)
    }

    /// Applies `PollReport`: executes `HostCommand`s, updates `ThemeTokens` only when
    /// `dirty.theme`, syncs modal inputs, and returns whether UI needs a redraw.
    pub(super) fn apply_poll_report(
        &mut self,
        report: PollReport,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let should_notify = report.should_notify_ui(
            self.chrome.fullscreen,
            self.chrome.chrome_visible,
            self.sidebar_is_visible() && self.catalog.sidebar_mode == SidebarMode::Plugins,
        );
        let PollReport {
            actions,
            notices,
            dirty,
            status_changed,
        } = report;

        if status_changed {
            self.locations.reload();
        }

        for action in actions {
            if action.is_live(self.plugins.generation()) {
                self.execute_one_host_command(&action, cx);
            }
        }
        for notice in notices {
            self.apply_plugin_notice(notice, cx);
        }
        if dirty.theme {
            self.sync_theme_tokens(cx);
        }
        if dirty.sources {
            self.reconcile_active_source();
            if !self.catalog.active_source.is_local() {
                self.rebuild_directory_rows();
            }
        }
        if dirty.modal {
            self.plugin_view.modal.on_dirty();
        }
        if self.plugin_view.modal.focus_pending
            && let Some(window) = window
        {
            self.sync_plugin_modal_focus(window);
        }
        self.sync_view_timer(cx);
        should_notify
    }

    pub(super) fn sync_plugin_modal_focus(&mut self, window: &mut Window) {
        if self.has_host_consent() {
            self.text_inputs.active_form_key = None;
            if !self.focus.is_focused(window) {
                self.focus.focus(window);
            }
            return;
        }
        if !std::mem::take(&mut self.plugin_view.modal.focus_pending) {
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

    fn execute_one_host_command(
        &mut self,
        action: &iris_plugin_host::ui::Action,
        cx: &mut Context<Self>,
    ) {
        let pid = &action.binding.installed.plugin_id;
        match action.command().clone() {
            HostCommand::TogglePlayback => self.command(Command::TogglePlayback, cx),
            HostCommand::SeekRelative { seconds } => {
                self.command(Command::SeekRelative(seconds), cx);
            }
            HostCommand::SeekAbsolute { seconds } => {
                self.command(Command::SeekAbsolute(seconds), cx);
            }
            HostCommand::SetSpeed { speed } => self.command(Command::SetSpeed(speed), cx),
            HostCommand::SetVolume { volume } => self.command(Command::SetVolume(volume), cx),
            HostCommand::ToggleMute => self.command(Command::ToggleMute, cx),
            HostCommand::OpenNext => self.step(1, true, cx),
            HostCommand::OpenPrevious => self.step(-1, true, cx),
            HostCommand::ToggleFavorite => {
                if self.active().session.may_share_current_identity()
                    && let Some(uri) = self.active().session.current_media_uri()
                {
                    self.store.toggle_favorite(&uri);
                    self.rebuild_directory_rows();
                    self.scan_visible_favorites(cx);
                }
            }
            HostCommand::ShowToast { message } => {
                if self.plugin_view.modal.clear_submitting_for(pid)
                    && self.plugins.active_modal().is_some_and(|(p, _)| p == pid)
                {
                    self.plugin_view.modal.error = Some(message.into());
                    cx.notify();
                } else {
                    self.show_toast(message, cx);
                }
            }
            HostCommand::SetAbLoop { start, end } => {
                self.command(Command::SetAbLoop { start, end }, cx)
            }
            HostCommand::ClearAbLoop => self.command(Command::ClearAbLoop, cx),
            HostCommand::RefreshSource { source_id } => {
                self.refresh_plugin_source(pid, &source_id, cx);
            }
            HostCommand::SetSidebar { .. }
            | HostCommand::SetStatus { .. }
            | HostCommand::SetOverlay { .. }
            | HostCommand::SetToolbar { .. }
            | HostCommand::OpenModal { .. }
            | HostCommand::CloseModal { .. }
            | HostCommand::SetTheme { .. }
            | HostCommand::SetSources { .. }
            | HostCommand::Log { .. } => {}
            _ => self.apply_plugin_enhancement(action, cx),
        }
    }

    /// Sends `HostEvent::ModalSubmit` while keeping the modal open and preserving
    /// user-entered `modal:*` fields until the plugin validates and replies `CloseModal`.
    pub(super) fn submit_active_plugin_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_host_consent() {
            return;
        }
        let Some((pid, modal)) = self
            .plugins
            .active_modal()
            .map(|(p, m)| (p.to_string(), m.clone()))
        else {
            return;
        };
        let Some(values) = self
            .plugin_view
            .modal
            .begin_submit(&self.text_inputs, &pid, &modal)
        else {
            return;
        };
        let report = self.plugins.send_to(
            &pid,
            HostEvent::ModalSubmit {
                id: modal.id,
                values,
            },
        );
        self.apply_poll_report(report, Some(window), cx);
        cx.notify();
    }

    /// Unconditionally dismisses the active plugin modal (`Esc` / Cancel / scrim click),
    /// clears `modal:*` fields, and notifies the plugin via `HostEvent::ModalCancel`.
    pub(super) fn cancel_active_plugin_modal(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some((pid, modal_id)) = self
            .plugins
            .active_modal()
            .map(|(p, m)| (p.to_string(), m.id.clone()))
        else {
            return;
        };
        self.plugins.dismiss_modal(&pid);
        let report = self
            .plugins
            .send_to(&pid, HostEvent::ModalCancel { id: modal_id });
        self.plugin_view.modal.cancel(&mut self.text_inputs);
        self.plugin_view.modal.focus_pending = true;
        // A dismissed front modal can reveal another queued modal without a dirty slot.
        self.apply_poll_report(report, window, cx);
        self.bump_activity(cx);
        cx.notify();
    }

    pub(super) fn invoke_plugin_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.grant_current_annotations(plugin_id, cx);
        let snap = self.plugin_snapshot();
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::Command {
                id: command_id.to_string(),
                snapshot: Some(snap),
            },
        );
        self.apply_poll_report(report, Some(window), cx);
        cx.notify();
    }

    pub(super) fn invoke_plugin_widget_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        value: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.grant_current_annotations(plugin_id, cx);
        let snap = self.plugin_snapshot();
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::Action {
                id: action_id.to_string(),
                value,
                snapshot: Some(snap),
            },
        );
        self.apply_poll_report(report, Some(window), cx);
        cx.notify();
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

    pub(super) fn cleanup_plugin_playback(&mut self, pid: Option<&str>, sid: Option<&str>) {
        self.cancel_plugin_requests(pid, sid);
        let uri_matches = matches!(
            self.active().session.current_media_uri(),
            Some(MediaUri::Plugin {
                plugin_id,
                source_id,
                ..
            }) if pid.is_none_or(|p| p == plugin_id) && sid.is_none_or(|s| s == source_id)
        );
        let meta_matches = self
            .catalog
            .active_stream_meta
            .as_ref()
            .is_some_and(|m| m.matches_source(pid, sid));
        let owner_matches = sid.is_none()
            && iris_plugin_host::stream::prepare::stream_session(&self.active().session)
                .is_some_and(|session| match pid {
                    None => true,
                    Some(target) => {
                        self.plugin_view.stream_plugin.as_deref() == Some(target)
                            || self
                                .plugin_view
                                .playlist_binding
                                .as_ref()
                                .is_some_and(|b| b.installed.plugin_id == target)
                            || self
                                .plugins
                                .list()
                                .iter()
                                .find(|p| p.id == target)
                                .and_then(|p| p.binding.as_ref())
                                .is_some_and(|b| {
                                    b.installed.installation_id == session.factory.owner()
                                })
                    }
                });
        let playlist_matches = sid.is_some_and(|target_sid| {
            self.plugin_view
                .playlist_binding
                .as_ref()
                .is_some_and(|b| pid.is_none_or(|p| b.installed.plugin_id == p))
                && self
                    .plugin_view
                    .playlist_current
                    .as_ref()
                    .is_some_and(|(cur_sid, _)| cur_sid == target_sid)
        });
        if uri_matches || meta_matches || owner_matches || playlist_matches {
            self.save_progress_sync();
            let _ = self.active_mut().session.stop();
            self.active_mut().session.release_scoped();
            self.plugin_view.scoped_released = false;
            self.active_mut().image.reset();
            self.sleep_preventer.set_active(false);
            self.sync_now_playing(true);
            self.plugin_view.stream_plugin = None;
            self.plugin_view.playlist_current = None;
            self.catalog.active_stream_meta = None;
            if self.overlays.popover == Some(PopoverKind::Variants) {
                self.overlays.popover = None;
                self.overlays.popover_index = None;
            }
        } else {
            self.catalog
                .clear_stream_meta_for(&mut self.overlays, pid, sid);
        }
        if self
            .plugin_view
            .playlist_binding
            .as_ref()
            .is_some_and(|b| pid.is_none_or(|p| b.installed.plugin_id == p))
        {
            if let Some(target_sid) = sid {
                self.plugin_view
                    .playlist
                    .retain(|entry| entry.source_id != target_sid);
                if self
                    .plugin_view
                    .playlist_current
                    .as_ref()
                    .is_some_and(|(cur_sid, _)| cur_sid == target_sid)
                {
                    self.plugin_view.playlist_current = None;
                }
                if self.plugin_view.playlist.is_empty() {
                    self.plugin_view.playlist_binding = None;
                    self.plugin_view.playlist_current = None;
                }
            } else {
                self.plugin_view.playlist.clear();
                self.plugin_view.playlist_binding = None;
                self.plugin_view.playlist_current = None;
            }
        }
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
            self.cleanup_plugin_playback(Some(id), None);
        }
        let snap = self.plugin_snapshot();
        let report = self.plugins.set_enabled_with_snapshot(id, enabled, snap);
        self.apply_poll_report(report, None, cx);
        self.reconcile_active_source();
        cx.notify();
    }

    pub(super) fn uninstall_plugin(&mut self, id: &str, keep_data: bool, cx: &mut Context<Self>) {
        if id.to_ascii_lowercase().starts_with("iris.") {
            self.show_toast("官方内置插件不可删除", cx);
            return;
        }
        self.preset_rt
            .set_plugin_enabled(&mut self.settings, id, true);
        self.text_inputs.clear_plugin_sidebar_fields(id);
        self.cleanup_plugin_playback(Some(id), None);
        self.plugins.dismiss_modal(id);
        if self.settings.theme_id == format!("plugin:{id}") {
            self.settings.theme_id = "graphite".into();
            self.persist_user_settings();
            self.sync_theme_tokens(cx);
        }
        let report = self.plugins.uninstall(id, keep_data);
        self.apply_poll_report(report, None, cx);
        self.reconcile_active_source();
        cx.notify();
    }

    pub(super) fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        self.cleanup_plugin_playback(None, None);
        let snap = self.plugin_snapshot();
        let report = self
            .plugins
            .reload_with_snapshot(&self.settings.disabled_plugins, snap);
        self.apply_poll_report(report, None, cx);
        let enabled_ids: Vec<String> = self
            .plugins
            .list()
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.id.clone())
            .collect();
        self.text_inputs
            .retain_enabled_sidebar_fields(|pid| enabled_ids.iter().any(|id| id == pid));
        self.reconcile_active_source();
        self.show_toast("插件列表已重新扫描", cx);
    }

    pub(super) fn handle_source_error(
        &mut self,
        plugin_id: &str,
        request_id: &str,
        message: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(kind) = self.catalog.take_error(plugin_id, request_id) else {
            return false;
        };
        match kind {
            SourceErrorKind::Resolve => {
                let vp = self.active_mut();
                vp.loading_visible = false;
                vp.pending_subtitles.clear();
            }
            SourceErrorKind::Browse { source_id } => {
                if let ActiveSource::Plugin(ps) = &mut self.catalog.active_source
                    && ps.plugin_id == plugin_id
                    && ps.source_id == source_id
                {
                    ps.error = Some(message.clone());
                    self.rebuild_directory_rows();
                }
            }
        }
        self.show_toast(format!("数据源错误 · {message}"), cx);
        true
    }
}
