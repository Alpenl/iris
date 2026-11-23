//! Plugin `PollReport` application, `DirtySlots` UI redraw gating, modal async
//! validation & focus synchronization, and `HostCommand` execution.

use gpui::{Context, Window};
use iris_api::{HostCommand, HostEvent, PlaybackSnapshot, SourcePage};
use iris_core::player::Command;
use iris_core::plugin::{self, PollReport};

use super::IrisWindow;
use super::state::ActiveSource;

impl IrisWindow {
    pub(super) fn plugin_snapshot(&self) -> PlaybackSnapshot {
        let session = &self.active().session;
        let fav = session
            .current_media_uri()
            .is_some_and(|u| self.store.is_favorite(&u));
        plugin::snapshot_from_session(session, fav)
    }

    pub(super) fn sidebar_is_visible(&self) -> bool {
        if self.chrome.fullscreen {
            self.overlays.dir_popup
        } else {
            self.settings.directory_open
        }
    }

    /// Drains background plugin messages (`cx.spawn_in` wakeup), auto-focusing
    /// newly opened modals and only notifying GPUI when `PollReport::should_notify_ui` holds.
    pub(super) fn poll_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let report = self.plugins.poll();
        if self.apply_poll_report(report, Some(window), cx) {
            cx.notify();
        }
    }

    pub(super) fn broadcast_plugin_event(
        &mut self,
        event: HostEvent,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
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
            self.sidebar_is_visible(),
        );
        let PollReport {
            commands, dirty, ..
        } = report;

        let had_modal_before = self.plugins.active_modal().is_some();
        for (pid, cmd) in commands {
            self.execute_one_host_command(&pid, cmd, cx);
        }
        if dirty.theme {
            self.sync_theme_tokens(cx);
        }
        if dirty.sources && !self.catalog.active_source.is_local() {
            self.rebuild_directory_rows();
        }
        if dirty.modal || (had_modal_before && self.plugins.active_modal().is_none()) {
            self.overlays.submitting_plugin_modal = None;
            let need_focus = self
                .text_inputs
                .sync_plugin_modal(self.plugins.active_modal());
            if let Some(w) = window {
                if need_focus {
                    self.text_inputs.form_focus.focus(w);
                } else if self.plugins.active_modal().is_none()
                    && !self.text_inputs.is_any_focused(w)
                {
                    self.focus.focus(w);
                }
            }
        }
        should_notify
    }

    fn execute_one_host_command(&mut self, pid: &str, cmd: HostCommand, cx: &mut Context<Self>) {
        match cmd {
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
                if let Some(uri) = self.active().session.current_media_uri() {
                    self.store.toggle_favorite(&uri);
                    self.rebuild_directory_rows();
                }
            }
            HostCommand::ShowToast { message } => {
                self.overlays.clear_submitting_for_plugin(pid);
                self.show_toast(message, cx);
            }
            HostCommand::ProvideSourcePage { request_id, page } => {
                self.handle_plugin_source_page(pid, &request_id, *page);
            }
            HostCommand::ResolveStream { request_id, stream } => {
                self.handle_plugin_resolved_stream(pid, request_id, *stream, cx);
            }
            HostCommand::SourceError {
                request_id,
                message,
            } => {
                self.overlays.clear_submitting_for_plugin(pid);
                self.active_mut().loading_visible = false;
                if let ActiveSource::Plugin(ps) = &mut self.catalog.active_source
                    && ps.plugin_id == pid
                    && (request_id.is_empty() || ps.active_request_id == request_id)
                {
                    ps.loading = false;
                    ps.error = Some(message.clone());
                    self.rebuild_directory_rows();
                }
                self.show_toast(format!("数据源错误 · {message}"), cx);
            }
            HostCommand::CloseModal { .. } => {
                self.plugins.dismiss_modal(pid);
                self.overlays.complete_plugin_modal_close(
                    &mut self.text_inputs,
                    self.plugins.active_modal(),
                );
            }
            HostCommand::SetState { .. }
            | HostCommand::SetSecret { .. }
            | HostCommand::SetSidebar { .. }
            | HostCommand::SetStatus { .. }
            | HostCommand::SetOverlay { .. }
            | HostCommand::SetToolbar { .. }
            | HostCommand::OpenModal { .. }
            | HostCommand::SetTheme { .. }
            | HostCommand::SetSources { .. } => {}
        }
    }

    /// Sends `HostEvent::ModalSubmit` while keeping the modal open and preserving
    /// user-entered `modal:*` fields until the plugin validates and replies `CloseModal`.
    pub(super) fn submit_active_plugin_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pid, modal)) = self
            .plugins
            .active_modal()
            .map(|(p, m)| (p.to_string(), m.clone()))
        else {
            return;
        };
        let Some(values) = self
            .overlays
            .begin_plugin_modal_submit(&self.text_inputs, &pid, &modal)
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
        self.overlays
            .cancel_plugin_modal_state(&mut self.text_inputs);
        let report = self
            .plugins
            .send_to(&pid, HostEvent::ModalCancel { id: modal_id });
        if let Some(w) = window {
            self.apply_poll_report(report, Some(w), cx);
            if self.plugins.active_modal().is_none() && !self.text_inputs.is_any_focused(w) {
                self.text_inputs.clear_modal_fields();
                self.focus.focus(w);
            }
        } else {
            self.apply_poll_report(report, None, cx);
            if self.plugins.active_modal().is_none() {
                self.text_inputs.clear_modal_fields();
            }
        }
        cx.notify();
    }

    pub(super) fn invoke_plugin_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snap = self.plugin_snapshot();
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::Command {
                id: command_id.to_string(),
                snapshot: snap,
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
        let snap = self.plugin_snapshot();
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::Action {
                id: action_id.to_string(),
                value,
                snapshot: snap,
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

    pub(super) fn toggle_plugin_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if enabled {
            self.settings.disabled_plugins.retain(|d| d != id);
        } else if !self.settings.disabled_plugins.iter().any(|d| d == id) {
            self.settings.disabled_plugins.push(id.to_string());
        }
        self.preset_rt.record_user_edit(&self.settings);
        let snap = self.plugin_snapshot();
        let report = self.plugins.set_enabled_with_snapshot(id, enabled, snap);
        self.apply_poll_report(report, None, cx);
        if !enabled
            && matches!(
                &self.catalog.active_source,
                ActiveSource::Plugin(ps) if ps.plugin_id == id
            )
        {
            self.catalog.active_source = ActiveSource::Local;
            self.rebuild_directory_rows();
        }
        cx.notify();
    }

    pub(super) fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        let snap = self.plugin_snapshot();
        let report = self
            .plugins
            .reload_with_snapshot(&self.settings.disabled_plugins, snap);
        self.apply_poll_report(report, None, cx);
        self.show_toast("插件列表已重新扫描", cx);
    }

    pub(super) fn handle_plugin_source_page(
        &mut self,
        plugin_id: &str,
        req_id: &str,
        page: SourcePage,
    ) {
        if let ActiveSource::Plugin(ps) = &mut self.catalog.active_source
            && ps.plugin_id == plugin_id
            && ps.source_id == page.source_id
            && (req_id.is_empty() || ps.active_request_id == req_id)
        {
            let appending = !ps.entries.is_empty() && ps.folder_id == page.folder_id;
            ps.loading = false;
            ps.error = None;
            ps.folder_id = page.folder_id;
            ps.parent_id = page.parent_id;
            ps.title = page.title;
            ps.authenticated = page.authenticated;
            ps.auth_hint = page.auth_hint;
            ps.next_page = page.next_page;
            if appending {
                ps.entries.extend(page.entries);
            } else {
                ps.entries = page.entries;
            }
            self.rebuild_directory_rows();
        }
    }
}
