//! Workspace preset switching (`PresetRuntime`), clean personal-baseline
//! persistence, effective chrome queries, and `ResetAppearance` guardrail.

use gpui::{Context, Window, px, size};
use iris_api::{HostEvent, Permission};
use iris_core::library::{SortBy, SortOrder};
use iris_core::player::Command;
use iris_core::preset::{PresetCaptureScope, PresetLayout, PresetManager, SidebarMode};

use super::IrisWindow;
use super::chrome::DirectoryPopup;
use crate::platform;
use crate::theme::ThemeTokens;

impl IrisWindow {
    pub(super) fn sync_theme_tokens(&self, cx: &mut Context<Self>) {
        let (plugin_theme, _) = self.plugins.active_ui_overrides();
        let tokens = ThemeTokens::resolve(&self.settings.theme_id, plugin_theme.as_ref());
        cx.set_global(tokens);
    }

    /// Persists a user-initiated settings edit to the clean personal baseline (`settings.conf`).
    pub(super) fn persist_user_settings(&mut self) {
        self.preset_rt.record_user_edit(&self.settings);
    }

    pub(super) fn effective_hide_titlebar(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome.and_then(|c| c.hide_titlebar).unwrap_or(false)
    }

    pub(super) fn effective_floating_dock(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.floating_dock)
            .unwrap_or(self.settings.floating_dock)
    }

    pub(super) fn effective_show_titlebar_meta(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.show_titlebar_meta)
            .unwrap_or(self.settings.show_titlebar_meta)
    }

    pub(super) fn effective_compact_controls(&self) -> bool {
        let (_, chrome) = self.plugins.active_ui_overrides();
        chrome
            .and_then(|c| c.compact_controls)
            .unwrap_or(self.settings.compact_controls)
    }

    pub(super) fn is_dock_button_hidden(&self, button_id: &str) -> bool {
        if self
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

    /// Guardrail #3: One-click safety escape that disables `UiTheme` plugins, clears
    /// frameless/chrome overrides, and restores the default Graphite theme.
    pub(super) fn reset_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlays.close_all();
        self.text_inputs.active_form_key = None;
        self.focus.focus(window);
        let theme_plugin_ids: Vec<String> = self
            .plugins
            .list()
            .into_iter()
            .filter(|p| p.enabled && p.manifest.has_permission(Permission::UiTheme))
            .map(|p| p.manifest.id)
            .collect();
        let snap = self.plugin_snapshot();
        for pid in theme_plugin_ids {
            if !self.settings.disabled_plugins.iter().any(|d| d == &pid) {
                self.settings.disabled_plugins.push(pid.clone());
            }
            let report = self
                .plugins
                .set_enabled_with_snapshot(&pid, false, snap.clone());
            self.apply_poll_report(report, Some(window), cx);
        }
        while let Some((plugin, _)) = self.plugins.active_modal() {
            let plugin = plugin.to_owned();
            self.plugins.dismiss_modal(&plugin);
        }
        self.overlays.plugin_modal_focus_pending = true;
        self.sync_plugin_modal_focus(window);
        self.settings.theme_id = "graphite".into();
        self.settings.floating_dock = false;
        self.settings.compact_controls = false;
        self.settings.show_titlebar_meta = true;
        self.settings.hidden_dock_buttons.clear();
        self.persist_user_settings();
        self.sync_theme_tokens(cx);
        self.show_toast("已恢复默认石墨外观", cx);
    }

    pub(super) fn set_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.catalog.sidebar_mode = mode;
        if !self.chrome.fullscreen {
            if !self.settings.directory_open {
                self.settings.directory_open = true;
                self.persist_user_settings();
            }
        } else {
            self.overlays.dir_popup = DirectoryPopup::Manual;
            self.bump_activity(cx);
        }
        cx.notify();
    }

    fn current_preset_layout(&self, window: &Window) -> PresetLayout {
        let size = window.viewport_size();
        PresetLayout {
            sidebar_mode: self.catalog.sidebar_mode,
            window_size: self.chrome.pending_window_size.or_else(|| {
                if window.is_fullscreen() || window.is_maximized() {
                    self.chrome.last_window_size
                } else {
                    Some((f32::from(size.width), f32::from(size.height)))
                }
            }),
        }
    }

    pub(super) fn activate_preset(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if id != "default" && self.presets.get(id).is_none() {
            self.show_toast(format!("未找到预设 · {id}"), cx);
            return;
        }
        let preset_name = self
            .presets
            .get(id)
            .map_or_else(|| "默认工作区".into(), |p| p.name.clone());
        let prev_disabled = self.settings.disabled_plugins.clone();
        let layout = self.current_preset_layout(window);
        let trans = self
            .preset_rt
            .switch_preset(&self.presets, id, &mut self.settings, layout);

        for (pid, cmd_id) in trans.on_exit_commands {
            let snap = self.plugin_snapshot();
            let report = self.plugins.send_to(
                &pid,
                HostEvent::Command {
                    id: cmd_id,
                    snapshot: snap,
                },
            );
            self.apply_poll_report(report, Some(window), cx);
        }

        if prev_disabled != self.settings.disabled_plugins {
            let all_ids: Vec<String> = self
                .plugins
                .list()
                .into_iter()
                .map(|p| p.manifest.id)
                .collect();
            let snap = self.plugin_snapshot();
            for pid in all_ids {
                let enable = !self.settings.disabled_plugins.iter().any(|d| d == &pid);
                let report = self
                    .plugins
                    .set_enabled_with_snapshot(&pid, enable, snap.clone());
                self.apply_poll_report(report, Some(window), cx);
            }
        }

        for (pid, cmd_id) in trans.plugin_commands {
            self.invoke_plugin_command(&pid, &cmd_id, window, cx);
        }

        if let Some(mode) = trans.sidebar_mode {
            self.catalog.sidebar_mode = mode;
        }
        platform::set_always_on_top(window, self.settings.always_on_top);
        if let Some((w, h)) = trans.window_size {
            if window.is_fullscreen() {
                self.chrome.pending_window_size = Some((w, h));
            } else {
                window.resize(size(px(w), px(h)));
                self.chrome.last_window_size = Some((w, h));
            }
        }
        if let Some(sp) = trans.speed {
            self.command(Command::SetSpeed(sp), cx);
        }
        if let Some(asp) = trans.aspect {
            self.command(Command::SetAspect(asp), cx);
        }

        self.active().session.apply_settings(&self.settings);
        self.sync_theme_tokens(cx);
        self.ensure_filmstrip_thumbnails(cx);
        self.overlays.popover = None;
        self.show_toast(format!("已切换预设 · {preset_name}"), cx);
    }

    pub(super) fn save_current_as_preset(
        &mut self,
        name: String,
        description: String,
        scope: PresetCaptureScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let vp = window.viewport_size();
        let win_sz =
            (!self.chrome.fullscreen).then_some((f32::from(vp.width), f32::from(vp.height)));
        let st = self.active().session.state();
        let preset = PresetManager::capture_from_current(
            String::new(),
            if name.trim().is_empty() {
                "自定义预设".into()
            } else {
                name.trim().to_string()
            },
            description.trim().to_string(),
            scope,
            &self.settings,
            self.preset_rt.baseline(),
            self.catalog.sidebar_mode,
            win_sz,
            st.speed,
            st.aspect,
        );
        let label = preset.name.clone();
        let saved_id = self.presets.save_custom(preset);
        self.activate_preset(&saved_id, window, cx);
        self.show_toast(format!("已保存预设 · {label}"), cx);
    }

    pub(super) fn update_preset_from_current(
        &mut self,
        id: &str,
        scope: PresetCaptureScope,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let vp = window.viewport_size();
        let win_sz =
            (!self.chrome.fullscreen).then_some((f32::from(vp.width), f32::from(vp.height)));
        let st = self.active().session.state();
        if self.presets.update_from_current(
            id,
            scope,
            &self.settings,
            self.preset_rt.baseline(),
            self.catalog.sidebar_mode,
            win_sz,
            st.speed,
            st.aspect,
        ) {
            self.show_toast("已将当前状态更新至预设", cx);
        } else {
            self.show_toast("内置预设不可直接覆盖，请另存为新预设", cx);
        }
    }

    pub(super) fn cycle_sort(&mut self, cx: &mut Context<Self>) {
        let (next_by, next_ord) = match (self.settings.sort_by, self.settings.sort_order) {
            (SortBy::Name, SortOrder::Asc) => (SortBy::Name, SortOrder::Desc),
            (SortBy::Name, SortOrder::Desc) => (SortBy::Modified, SortOrder::Desc),
            (SortBy::Modified, SortOrder::Desc) => (SortBy::Modified, SortOrder::Asc),
            (SortBy::Modified, SortOrder::Asc) => (SortBy::Size, SortOrder::Desc),
            (SortBy::Size, SortOrder::Desc) => (SortBy::Size, SortOrder::Asc),
            (SortBy::Size, SortOrder::Asc) => (SortBy::Name, SortOrder::Asc),
        };
        self.settings.sort_by = next_by;
        self.settings.sort_order = next_ord;
        self.persist_user_settings();
        self.rebuild_directory_rows();
        self.ensure_filmstrip_thumbnails(cx);
        let by_str = match next_by {
            SortBy::Name => "名称",
            SortBy::Modified => "修改时间",
            SortBy::Size => "文件大小",
        };
        let ord_str = if next_ord == SortOrder::Asc {
            "升序"
        } else {
            "降序"
        };
        self.show_toast(format!("排序 · {by_str} {ord_str}"), cx);
    }
}
