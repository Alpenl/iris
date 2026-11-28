//! GPUI workbench bridge: applies [`iris_app::WorkbenchDomainChanges`] onto
//! `ThemeTokens`, `rebind_keys`, window resize / `always_on_top`, and regional
//! `DirtyRegions`, delegating all preset/settings/baseline logic to [`iris_app::AppModel`].

use std::time::Instant;

use gpui::{Context, Window, px, size};
use iris_app::WorkbenchDomainChanges;
use iris_core::library::{SortBy, SortOrder};
use iris_core::preset::{PresetCaptureScope, SidebarMode};

use super::WindowShell;
use super::actions::rebind_keys;
use super::chrome::DirectoryPopup;
use super::views::{DirtyRegions, dirty_regions_for_workbench};
use crate::platform;
use crate::theme::ThemeTokens;

impl WindowShell {
    pub(super) fn sync_theme_tokens(&mut self, cx: &mut Context<Self>) {
        let (theme_id, plugin_theme, scale) = self.app.select_active_theme();
        let tokens = ThemeTokens::resolve_scaled(&theme_id, plugin_theme.as_ref(), scale);
        cx.set_global(tokens);
    }

    /// Applies pure headless [`WorkbenchDomainChanges`] onto GPUI globals, key bindings,
    /// platform window attributes, and returns the live [`DirtyRegions`] set.
    pub(in crate::ui) fn apply_workbench_changes(
        &mut self,
        changes: WorkbenchDomainChanges,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        if changes.keymap_changed {
            rebind_keys(cx, self.keymap());
        }
        if let Some(pinned) = changes.always_on_top_changed
            && let Some(w) = window.as_deref_mut()
        {
            platform::set_always_on_top(w, pinned);
        }
        if let Some((rw, rh)) = changes.window_resize {
            if self.chrome.fullscreen || window.as_deref().is_some_and(Window::is_fullscreen) {
                self.chrome.pending_window_size = Some((rw, rh));
            } else if let Some(w) = window {
                w.resize(size(px(rw), px(rh)));
                self.chrome.last_window_size = Some((rw, rh));
            }
        }
        if changes.theme_changed {
            self.sync_theme_tokens(cx);
        }
        if changes.theme_changed
            || changes.root_layout_changed
            || changes.dock_changed
            || changes.viewport_changed
        {
            self.sync_viewport_effects(cx);
        }
        dirty_regions_for_workbench(&changes, self.ui_visibility())
    }

    pub(super) fn effective_hide_titlebar(&self) -> bool {
        self.app.effective_hide_titlebar()
    }

    pub(super) fn effective_floating_dock(&self) -> bool {
        self.app.effective_floating_dock()
    }

    pub(super) fn effective_show_titlebar_meta(&self) -> bool {
        self.app.effective_show_titlebar_meta()
    }

    pub(super) fn effective_compact_controls(&self) -> bool {
        self.app.effective_compact_controls()
    }

    pub(super) fn is_dock_button_hidden(&self, button_id: &str) -> bool {
        self.app.is_dock_button_hidden(button_id)
    }

    /// Guardrail #3: One-click safety escape that disables `UiTheme` plugins, clears
    /// frameless/chrome overrides, and restores the default Graphite theme.
    pub(super) fn reset_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlays.close_all();
        self.text_inputs.active_form_key = None;
        self.focus.focus(window);
        let outcome = self.app.reset_appearance(Instant::now());
        for pid in &outcome.cleared_sidebar_plugins {
            self.text_inputs.clear_plugin_sidebar_fields(pid);
        }
        let plugin_regions = self.apply_plugin_outcome(outcome.plugin_outcome, Some(window), cx);
        let wb_regions = self.apply_workbench_changes(outcome.workbench, Some(window), cx);
        self.notify_regions(plugin_regions.union(wb_regions), cx);
        self.show_toast("已恢复默认石墨外观", cx);
    }

    pub(super) fn set_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.app.set_sidebar_mode(mode);
        let mut regions = DirtyRegions {
            sidebar: true,
            plugin_panel: mode == SidebarMode::Plugins,
            ..DirtyRegions::NONE
        };
        if !self.chrome.fullscreen {
            if let Some(wb) = self.app.ensure_docked_directory_open() {
                regions = regions.union(self.apply_workbench_changes(wb, None, cx));
            }
        } else {
            self.overlays.dir_popup = DirectoryPopup::Manual;
            self.bump_activity(cx);
            regions = DirtyRegions::ALL;
        }
        self.sync_view_timer(cx);
        self.notify_regions(regions, cx);
    }

    fn current_windowed_size(&self, window: &Window) -> Option<(f32, f32)> {
        let size = window.viewport_size();
        self.chrome.pending_window_size.or_else(|| {
            if window.is_fullscreen() || window.is_maximized() {
                self.chrome.last_window_size
            } else {
                Some((f32::from(size.width), f32::from(size.height)))
            }
        })
    }

    pub(super) fn activate_preset(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preset_name =
            super::strings::presets::preset_name_by_id(self.presets(), id).to_string();
        let win_sz = self.current_windowed_size(window);
        let Some(outcome) = self.app.activate_preset(id, win_sz, Instant::now()) else {
            self.show_toast(format!("未找到预设 · {id}"), cx);
            return;
        };
        for pid in &outcome.cleared_sidebar_plugins {
            self.text_inputs.clear_plugin_sidebar_fields(pid);
        }
        for err in &outcome.command_errors {
            log::warn!("preset player command failed: {err:#}");
            self.show_error_toast(super::strings::errors::player_action_error(err), cx);
        }
        let plugin_regions = self.apply_plugin_outcome(outcome.plugin_outcome, Some(window), cx);
        let wb_regions = self.apply_workbench_changes(outcome.workbench, Some(window), cx);
        self.rebuild_directory_rows();
        self.ensure_filmstrip_thumbnails(cx);
        self.overlays.popover = None;
        self.sync_view_timer(cx);
        self.notify_regions(
            plugin_regions.union(wb_regions).union(DirtyRegions::ALL),
            cx,
        );
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
        let win_sz = self.current_windowed_size(window);
        let saved = self.app.save_current_as_preset(
            name,
            description,
            scope,
            super::strings::presets::DEFAULT_CUSTOM_PRESET_NAME,
            win_sz,
            Instant::now(),
        );
        for pid in &saved.switched.cleared_sidebar_plugins {
            self.text_inputs.clear_plugin_sidebar_fields(pid);
        }
        for err in &saved.switched.command_errors {
            log::warn!("preset player command failed: {err:#}");
            self.show_error_toast(super::strings::errors::player_action_error(err), cx);
        }
        let plugin_regions =
            self.apply_plugin_outcome(saved.switched.plugin_outcome, Some(window), cx);
        let wb_regions = self.apply_workbench_changes(saved.switched.workbench, Some(window), cx);
        self.rebuild_directory_rows();
        self.ensure_filmstrip_thumbnails(cx);
        self.overlays.popover = None;
        self.sync_view_timer(cx);
        self.notify_regions(
            plugin_regions.union(wb_regions).union(DirtyRegions::ALL),
            cx,
        );
        self.show_toast(format!("已保存预设 · {}", saved.saved_name), cx);
    }

    pub(super) fn update_preset_from_current(
        &mut self,
        id: &str,
        scope: PresetCaptureScope,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let win_sz = self.current_windowed_size(window);
        if let Some(wb) = self.app.update_preset_from_current(id, scope, win_sz) {
            let regions = self.apply_workbench_changes(wb, None, cx);
            self.notify_regions(regions, cx);
            self.show_toast("已将当前状态更新至预设", cx);
        } else {
            self.show_toast("内置预设不可直接覆盖，请另存为新预设", cx);
        }
    }

    pub(super) fn delete_custom_preset(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let win_sz = self.current_windowed_size(window);
        let Some(deleted) = self.app.delete_custom_preset(id, win_sz, Instant::now()) else {
            return;
        };
        let mut regions = DirtyRegions::NONE;
        if let Some(switched) = deleted.fallback_switch {
            for pid in &switched.cleared_sidebar_plugins {
                self.text_inputs.clear_plugin_sidebar_fields(pid);
            }
            for err in &switched.command_errors {
                log::warn!("preset player command failed: {err:#}");
                self.show_error_toast(super::strings::errors::player_action_error(err), cx);
            }
            regions = regions
                .union(self.apply_plugin_outcome(switched.plugin_outcome, Some(window), cx))
                .union(DirtyRegions::ALL);
            self.rebuild_directory_rows();
            self.ensure_filmstrip_thumbnails(cx);
            self.sync_view_timer(cx);
        }
        let wb_regions = self.apply_workbench_changes(deleted.workbench, Some(window), cx);
        self.notify_regions(regions.union(wb_regions), cx);
        self.show_toast("已删除自定义工作区预设", cx);
    }

    pub(super) fn cycle_sort(&mut self, cx: &mut Context<Self>) {
        let (next_by, next_ord, wb) = self.app.cycle_sort();
        if !self.catalog().is_favorite_scan_in_flight() {
            self.catalog_ui.favorite_scan = None;
        }
        self.ensure_filmstrip_thumbnails(cx);
        let regions = self.apply_workbench_changes(wb, None, cx);
        self.notify_regions(regions, cx);
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
