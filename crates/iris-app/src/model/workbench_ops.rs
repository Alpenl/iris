//! Workbench settings, sparse preset switching, `ResetAppearance`, effective chrome
//! queries, and shortcut recording commands on [`super::AppModel`].

use std::time::Instant;

use iris_api::UiThemeSpec;
use iris_core::keymap::KeymapScheme;
use iris_core::player::Command;
use iris_core::preset::{PresetCaptureScope, PresetLayout, SidebarMode};
use iris_core::settings::{Hwdec, OnEof, PhotoDefault, Side, SubLang, VideoWheel};

use super::AppModel;
use crate::playback::{PlaybackActionOutcome, PlaybackSessionPort};
use crate::plugins::{PluginBatchOutcome, PluginPlayer};
use crate::workbench::{ShortcutRecordOutcome, WorkbenchDomainChanges, doh_resolver_preset_for};

/// Combined multi-domain outcome of switching workspace presets (`activate_preset`).
#[derive(Debug)]
pub struct PresetSwitchOutcome {
    pub preset_id: String,
    pub cleared_sidebar_plugins: Vec<String>,
    pub plugin_outcome: PluginBatchOutcome,
    pub command_errors: Vec<anyhow::Error>,
    pub workbench: WorkbenchDomainChanges,
}

/// Combined outcome of saving current state as a custom preset and activating it.
#[derive(Debug)]
pub struct PresetSaveOutcome {
    pub saved_id: String,
    pub saved_name: String,
    pub switched: PresetSwitchOutcome,
}

/// Combined outcome of deleting a custom preset (including fallback to `"default"` when active).
#[derive(Debug)]
pub struct PresetDeleteOutcome {
    pub deleted_id: String,
    pub fallback_switch: Option<PresetSwitchOutcome>,
    pub workbench: WorkbenchDomainChanges,
}

/// Combined outcome of the `ResetAppearance` emergency shortcut (`⌘⌥⇧R` / `Ctrl+Alt+Shift+R`).
#[derive(Debug)]
pub struct ResetAppearanceOutcome {
    pub cleared_sidebar_plugins: Vec<String>,
    pub plugin_outcome: PluginBatchOutcome,
    pub workbench: WorkbenchDomainChanges,
}

impl<S> AppModel<S> {
    /// Selects the active plugin theme (when `theme_id` has `"plugin:"` prefix) and returns
    /// `(theme_id, plugin_theme_spec, ui_font_scale)` for resolving GPUI `ThemeTokens`.
    pub fn select_active_theme(&mut self) -> (String, Option<UiThemeSpec>, f32) {
        let theme_id = self.workbench.settings.theme_id.clone();
        let (plugin_theme, _) = self.plugins.select_theme(&theme_id);
        (
            theme_id,
            plugin_theme,
            self.workbench.settings.ui_font_scale,
        )
    }

    pub fn effective_hide_titlebar(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome.and_then(|c| c.hide_titlebar).unwrap_or(false)
    }

    pub fn effective_floating_dock(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.floating_dock)
            .unwrap_or(self.workbench.settings.floating_dock)
    }

    pub fn effective_show_titlebar_meta(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.show_titlebar_meta)
            .unwrap_or(self.workbench.settings.show_titlebar_meta)
    }

    pub fn effective_compact_controls(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.compact_controls)
            .unwrap_or(self.workbench.settings.compact_controls)
    }

    pub fn is_dock_button_hidden(&self, button_id: &str) -> bool {
        if self
            .workbench
            .settings
            .hidden_dock_buttons
            .iter()
            .any(|b| b.eq_ignore_ascii_case(button_id))
        {
            return true;
        }
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome.is_some_and(|c| {
            c.hidden_dock_buttons
                .iter()
                .any(|b| b.eq_ignore_ascii_case(button_id))
        })
    }

    pub fn doh_resolver_preset(&self) -> u8 {
        doh_resolver_preset_for(&self.workbench.settings.doh_resolvers)
    }

    pub fn next_preset_id(&self) -> String {
        self.workbench.next_preset_id()
    }

    pub fn key_hint_string(&self, action_id: &str, is_macos: bool) -> Option<String> {
        self.workbench.key_hint_string(action_id, is_macos)
    }

    pub fn set_theme_id(&mut self, theme_id: &str) -> WorkbenchDomainChanges {
        self.workbench.set_theme_id(theme_id)
    }

    pub fn step_ui_font_scale(&mut self, delta: f32) -> WorkbenchDomainChanges {
        self.workbench.step_ui_font_scale(delta)
    }

    pub fn reset_ui_font_scale(&mut self) -> WorkbenchDomainChanges {
        self.workbench.reset_ui_font_scale()
    }

    pub fn set_directory_side(&mut self, side: Side) -> Option<WorkbenchDomainChanges> {
        self.workbench.set_directory_side(side)
    }

    pub fn step_directory_width(&mut self, delta: f32) -> WorkbenchDomainChanges {
        self.workbench.step_directory_width(delta)
    }

    pub fn reset_directory_width(&mut self) -> WorkbenchDomainChanges {
        self.workbench.reset_directory_width()
    }

    pub fn drag_sidebar_width(
        &mut self,
        pointer_x: f32,
        viewport_w: f32,
        start_x: f32,
        start_w: f32,
    ) -> WorkbenchDomainChanges {
        self.workbench
            .drag_sidebar_width(pointer_x, viewport_w, start_x, start_w)
    }

    pub fn commit_directory_width(&mut self) -> WorkbenchDomainChanges {
        self.workbench.commit_directory_width()
    }

    pub fn toggle_docked_directory(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.workbench.toggle_docked_directory()
    }

    pub fn ensure_docked_directory_open(&mut self) -> Option<WorkbenchDomainChanges> {
        self.workbench.ensure_docked_directory_open()
    }

    pub fn set_sidebar_mode(&mut self, mode: SidebarMode) {
        self.catalog.set_sidebar_mode(mode);
    }

    pub fn cycle_sidebar_mode(&mut self) -> SidebarMode {
        self.catalog.cycle_sidebar_mode()
    }

    pub fn toggle_floating_dock(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_floating_dock()
    }

    pub fn toggle_compact_controls(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_compact_controls()
    }

    pub fn toggle_show_titlebar_meta(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_show_titlebar_meta()
    }

    pub fn toggle_remember_window_size(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_remember_window_size()
    }

    pub fn toggle_always_on_top(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.workbench.toggle_always_on_top()
    }

    pub fn toggle_dock_button_visibility(&mut self, button_id: &str) -> WorkbenchDomainChanges {
        self.workbench.toggle_dock_button_visibility(button_id)
    }

    pub fn toggle_fake_ip_doh_fallback(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.workbench.toggle_fake_ip_doh_fallback()
    }

    pub fn set_doh_resolver_preset(&mut self, preset: u8) -> Option<WorkbenchDomainChanges> {
        self.workbench.set_doh_resolver_preset(preset)
    }

    pub fn set_seek_step(&mut self, secs: f64) -> WorkbenchDomainChanges {
        self.workbench.set_seek_step(secs)
    }

    pub fn toggle_single_click_pause(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_single_click_pause()
    }

    pub fn toggle_resume_playback(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_resume_playback()
    }

    pub fn toggle_remember_speed(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_remember_speed()
    }

    pub fn toggle_screenshot_subs(&mut self) -> WorkbenchDomainChanges {
        self.workbench.toggle_screenshot_subs()
    }

    pub fn set_video_wheel(&mut self, val: VideoWheel) -> WorkbenchDomainChanges {
        self.workbench.set_video_wheel(val)
    }

    pub fn set_fullscreen_hide_ms(&mut self, ms: u64) -> WorkbenchDomainChanges {
        self.workbench.set_fullscreen_hide_ms(ms)
    }

    pub fn set_photo_default(&mut self, val: PhotoDefault) -> WorkbenchDomainChanges {
        self.workbench.set_photo_default(val)
    }

    pub fn set_photo_double_click_actual(&mut self, val: bool) -> WorkbenchDomainChanges {
        self.workbench.set_photo_double_click_actual(val)
    }

    pub fn toggle_photo_filmstrip(&mut self) -> (bool, WorkbenchDomainChanges) {
        self.workbench.toggle_photo_filmstrip()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_shortcut_keystroke(
        &mut self,
        target_id: &str,
        context_pred: &str,
        key: &str,
        modifiers_cmd: bool,
        modifiers_ctrl: bool,
        modifiers_alt: bool,
        modifiers_shift: bool,
        is_macos: bool,
    ) -> ShortcutRecordOutcome {
        self.workbench.record_shortcut_keystroke(
            target_id,
            context_pred,
            key,
            modifiers_cmd,
            modifiers_ctrl,
            modifiers_alt,
            modifiers_shift,
            is_macos,
        )
    }

    pub fn record_normalized_shortcut(
        &mut self,
        target_id: &str,
        context_pred: &str,
        spec: &str,
        is_macos: bool,
    ) -> ShortcutRecordOutcome {
        self.workbench
            .record_normalized_shortcut(target_id, context_pred, spec, is_macos)
    }

    pub fn reset_shortcut_binding(&mut self, target_id: &str) -> WorkbenchDomainChanges {
        self.workbench.reset_shortcut_binding(target_id)
    }

    pub fn reset_all_shortcuts(&mut self) -> WorkbenchDomainChanges {
        self.workbench.reset_all_shortcuts()
    }

    pub fn set_keymap_scheme(&mut self, scheme: KeymapScheme) -> WorkbenchDomainChanges {
        self.workbench.set_keymap_scheme(scheme)
    }
}

impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    pub fn set_on_eof(&mut self, val: OnEof) -> WorkbenchDomainChanges {
        let changes = self.workbench.set_on_eof(val);
        self.playback.session_mut().set_on_eof_loop(val);
        changes
    }

    pub fn set_hwdec(&mut self, val: Hwdec) -> WorkbenchDomainChanges {
        let changes = self.workbench.set_hwdec(val);
        self.playback.session_mut().set_hwdec(val);
        changes
    }

    pub fn set_sub_lang(&mut self, val: SubLang) -> WorkbenchDomainChanges {
        let changes = self.workbench.set_sub_lang(val);
        self.playback.session_mut().set_sub_lang(val);
        changes
    }

    pub fn step_sub_scale(&mut self, delta: f64) -> WorkbenchDomainChanges {
        let (scale, changes) = self.workbench.step_sub_scale(delta);
        self.playback.session_mut().set_sub_scale(scale);
        changes
    }

    pub fn reset_sub_scale(&mut self) -> WorkbenchDomainChanges {
        let (scale, changes) = self.workbench.reset_sub_scale();
        self.playback.session_mut().set_sub_scale(scale);
        changes
    }

    pub fn set_audio_device(
        &mut self,
        dev_name: String,
    ) -> (PlaybackActionOutcome, WorkbenchDomainChanges) {
        let changes = self.workbench.set_audio_device(dev_name.clone());
        let playback_out = self
            .playback
            .execute_command(Command::SetAudioDevice(dev_name));
        (playback_out, changes)
    }

    /// Switches to `id` (`"default"` or any built-in/custom preset), coordinating `PresetRuntime`,
    /// DoH sync, exit/entry plugin hooks, plugin enable/disable deltas, sidebar mode & sort sync,
    /// and playback session settings.
    pub fn activate_preset(
        &mut self,
        id: &str,
        window_size: Option<(f32, f32)>,
        now: Instant,
    ) -> Option<PresetSwitchOutcome> {
        let current_layout = PresetLayout {
            sidebar_mode: self.catalog.sidebar_mode(),
            window_size,
        };
        let step = self.workbench.switch_preset(id, current_layout)?;
        let preset_id = step.transition.preset_id.clone();
        let mut plugin_outcome = PluginBatchOutcome::default();
        let mut cleared_sidebar_plugins = Vec::new();
        let mut command_errors = Vec::new();

        if !step.transition.on_exit_commands.is_empty() {
            let exit_cmds = step.transition.on_exit_commands;
            let out =
                self.with_plugin_ctx(|ctrl, ctx| ctrl.send_exit_commands(ctx, exit_cmds, now));
            plugin_outcome.merge(out);
        }

        if step.prev_disabled_plugins != self.workbench.settings.disabled_plugins {
            let all_ids: Vec<String> = self.plugins.list().iter().map(|p| p.id.clone()).collect();
            for pid in all_ids {
                let enable = !self
                    .workbench
                    .settings
                    .disabled_plugins
                    .iter()
                    .any(|d| d == &pid);
                if enable != step.prev_disabled_plugins.iter().any(|d| d == &pid) {
                    continue;
                }
                if !enable {
                    cleared_sidebar_plugins.push(pid.clone());
                }
                let out = self.with_plugin_ctx(|ctrl, ctx| {
                    ctrl.toggle_plugin_enabled(ctx, &pid, enable, now)
                });
                plugin_outcome.merge(out);
            }
        }

        if !step.transition.plugin_commands.is_empty() {
            let entry_cmds = step.transition.plugin_commands;
            let out = self
                .with_plugin_ctx(|ctrl, ctx| ctrl.queue_deferred_commands(ctx, entry_cmds, now));
            plugin_outcome.merge(out);
        }

        if let Some(mode) = step.transition.sidebar_mode {
            self.catalog.set_sidebar_mode(mode);
        }
        if (self.catalog.sort_by(), self.catalog.sort_order())
            != (
                self.workbench.settings.sort_by,
                self.workbench.settings.sort_order,
            )
        {
            let cur_uri = PlaybackSessionPort::current_media_uri(self.playback.session());
            let cur_local = PlaybackSessionPort::current(self.playback.session());
            self.catalog.set_sort(
                self.workbench.settings.sort_by,
                self.workbench.settings.sort_order,
                &self.plugins,
                cur_uri.as_ref(),
                cur_local,
            );
        }

        if let Some(sp) = step.transition.speed
            && let Some(err) = self.playback.execute_command(Command::SetSpeed(sp)).error
        {
            command_errors.push(err);
        }
        if let Some(asp) = step.transition.aspect
            && let Some(err) = self.playback.execute_command(Command::SetAspect(asp)).error
        {
            command_errors.push(err);
        }

        let active_settings = self.workbench.settings.clone();
        self.playback.session_mut().apply_settings(&active_settings);
        let mut workbench = step.changes;
        if plugin_outcome.dirty_slots.theme || plugin_outcome.domain.theme_changed {
            workbench.theme_changed = true;
        }
        if self.reconcile_missing_plugin_theme() {
            workbench.theme_changed = true;
        }

        Some(PresetSwitchOutcome {
            preset_id,
            cleared_sidebar_plugins,
            plugin_outcome,
            command_errors,
            workbench,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_current_as_preset(
        &mut self,
        name: String,
        description: String,
        scope: PresetCaptureScope,
        default_name: &str,
        window_size: Option<(f32, f32)>,
        now: Instant,
    ) -> PresetSaveOutcome {
        let (speed, aspect) = {
            let st = PlaybackSessionPort::state(self.playback.session());
            (st.speed, st.aspect)
        };
        let saved = self.workbench.capture_and_save_custom_preset(
            name,
            description,
            scope,
            default_name,
            self.catalog.sidebar_mode(),
            window_size,
            speed,
            aspect,
        );
        let switched = self
            .activate_preset(&saved.saved_id, window_size, now)
            .expect("newly saved custom preset exists");
        PresetSaveOutcome {
            saved_id: saved.saved_id,
            saved_name: saved.saved_name,
            switched,
        }
    }

    pub fn update_preset_from_current(
        &mut self,
        id: &str,
        scope: PresetCaptureScope,
        window_size: Option<(f32, f32)>,
    ) -> Option<WorkbenchDomainChanges> {
        let (speed, aspect) = {
            let st = PlaybackSessionPort::state(self.playback.session());
            (st.speed, st.aspect)
        };
        self.workbench.update_custom_preset(
            id,
            scope,
            self.catalog.sidebar_mode(),
            window_size,
            speed,
            aspect,
        )
    }

    pub fn delete_custom_preset(
        &mut self,
        id: &str,
        window_size: Option<(f32, f32)>,
        now: Instant,
    ) -> Option<PresetDeleteOutcome> {
        let step = self.workbench.delete_custom_preset(id)?;
        let mut workbench = step.changes;
        let fallback_switch = if step.was_active {
            let switched = self.activate_preset("default", window_size, now);
            if let Some(ref sw) = switched {
                workbench.merge(sw.workbench.clone());
            }
            switched
        } else {
            None
        };
        Some(PresetDeleteOutcome {
            deleted_id: step.deleted_id,
            fallback_switch,
            workbench,
        })
    }

    /// Guardrail #3: disables all active `UiTheme` plugins, dismisses plugin modals, clears
    /// chrome/button overrides, and restores the default Graphite theme.
    pub fn reset_appearance(&mut self, now: Instant) -> ResetAppearanceOutcome {
        let plugins = &mut self.plugins;
        let player = self.playback.session_mut();
        let settings = &mut self.workbench.settings;
        let (cleared_sidebar_plugins, plugin_outcome) =
            self.catalog.with_plugin_parts(|store, active_source| {
                plugins.reset_theme_plugins_and_modals(player, store, settings, active_source, now)
            });
        let workbench = self.workbench.finish_appearance_reset();
        ResetAppearanceOutcome {
            cleared_sidebar_plugins,
            plugin_outcome,
            workbench,
        }
    }

    pub(super) fn reconcile_missing_plugin_theme(&mut self) -> bool {
        let plugins = &self.plugins;
        self.workbench
            .ensure_valid_plugin_theme(|pid| plugins.list().iter().any(|p| p.id == pid))
    }
}
