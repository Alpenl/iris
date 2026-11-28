//! Window, preset, sidebar, overlay, screenshot, and photo-view action handlers.

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{Context, PathPromptOptions, Pixels, Point, Window};
use iris_app::ActiveSource;
use iris_core::library::Kind;
use iris_core::player::Command;
use iris_core::preset::SidebarMode;

use super::WindowShell;
use super::actions::{
    ActivatePreset, CycleAspect, CyclePreset, ExitFullscreen, FitImage, OpenCommandPalette,
    OpenFile, OpenSettings, OpenStreamUrl, RefreshDirectory, RotateCcw, RotateCw, TakeScreenshot,
    ToggleAlwaysOnTop, ToggleDirectory, ToggleFavorite, ToggleFilmstrip, ToggleFullscreen,
    ToggleHFlip, ToggleImageSize, ToggleInfo, ToggleSearch, ToggleVFlip, TriggerPluginCommand,
    ZoomIn, ZoomOut,
};
use super::chrome::DirectoryPopup;
use super::state::{BuiltinDialog, EscapeStep};
use super::views::DirtyRegions;
use crate::platform;

fn format_screenshot_stamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs() as i64;
    let ms = dur.subsec_millis();
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (hour, min, sec) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{day:02}-{hour:02}{min:02}{sec:02}-{ms:03}")
}

impl WindowShell {
    pub(super) fn toggle_favorite(
        &mut self,
        _: &ToggleFavorite,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session().current_media_uri().is_none() {
            return;
        }
        if !self.session().may_share_current_identity() {
            self.show_toast("此链接仅用于本次播放，无法收藏", cx);
            return;
        }
        let Some((_, added)) = self.app.toggle_current_favorite() else {
            return;
        };
        if !self.catalog().is_favorite_scan_in_flight() {
            self.catalog_ui.favorite_scan = None;
        }
        self.scan_visible_favorites(cx);
        self.notify_regions(
            DirtyRegions {
                sidebar: true,
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
        self.show_toast(
            if added {
                "已收藏"
            } else {
                "已取消收藏"
            },
            cx,
        );
    }

    pub(super) fn open_stream_url_action(
        &mut self,
        _: &OpenStreamUrl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        self.open_builtin_dialog(BuiltinDialog::OpenUrl, window, cx);
    }

    pub(super) fn cycle_preset_action(
        &mut self,
        _: &CyclePreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        let next = self.app.next_preset_id();
        self.activate_preset(&next, window, cx);
    }

    pub(super) fn activate_preset_action(
        &mut self,
        act: &ActivatePreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        self.activate_preset(&act.id, window, cx);
    }

    pub(super) fn trigger_plugin_command_action(
        &mut self,
        act: &TriggerPluginCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        self.invoke_plugin_command(&act.plugin_id, &act.command_id, window, cx);
    }

    pub(super) fn toggle_always_on_top(
        &mut self,
        _: &ToggleAlwaysOnTop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (pinned, wb) = self.app.toggle_always_on_top();
        let regions = self.apply_workbench_changes(wb, Some(window), cx);
        self.notify_regions(regions, cx);
        self.show_toast(
            if pinned {
                "窗口已置顶"
            } else {
                "已取消窗口置顶"
            },
            cx,
        );
    }

    pub(super) fn toggle_info(&mut self, _: &ToggleInfo, _: &mut Window, cx: &mut Context<Self>) {
        self.overlays.info_open = !self.overlays.info_open;
        if self.overlays.info_open {
            self.app.refresh_cached_stats(true, Instant::now());
            self.refresh_annotations(cx);
        }
        self.notify_viewport_and_dock(cx);
    }

    pub(super) fn take_screenshot(
        &mut self,
        _: &TakeScreenshot,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.session();
        if session.kind() == Some(Kind::Audio) {
            self.show_toast("当前为音频文件，无法截图", cx);
            return;
        }
        if session.current().is_none() && session.current_uri().is_none() {
            return;
        }
        let mut stem = String::new();
        if let Some(name) = session.current().and_then(Path::file_stem) {
            for ch in name
                .to_string_lossy()
                .chars()
                .map(|ch| if ch.is_control() { '_' } else { ch })
            {
                if stem.len() + ch.len_utf8() > 180 {
                    break;
                }
                stem.push(ch);
            }
        } else if let Some(title) = session.current_title() {
            stem.extend(
                title
                    .chars()
                    .take(24)
                    .map(|ch| if ch.is_alphanumeric() { ch } else { '_' }),
            );
        }
        let stem = if stem.is_empty() { "iris" } else { &stem };
        let out = platform::default_screenshot_dir()
            .join(format!("Iris-{stem}-{}.png", format_screenshot_stamp()));
        let with_subs = self.settings().screenshot_subs;
        if let Some(e) = self
            .app
            .execute_player_command(Command::Screenshot {
                path: out.clone(),
                with_subs,
            })
            .error
        {
            log::warn!("screenshot failed: {e:#}");
            self.show_error_toast("截图失败，请稍后重试", cx);
            return;
        }
        let name = out.file_name().unwrap_or_default().to_string_lossy();
        self.show_toast(format!("已请求截图 · {name}"), cx);
    }

    pub(super) fn cycle_aspect(&mut self, _: &CycleAspect, _: &mut Window, cx: &mut Context<Self>) {
        let out = self.app.cycle_aspect();
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_directory(
        &mut self,
        _: &ToggleDirectory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        self.overlays.close_menus();
        if self.chrome.fullscreen {
            self.overlays.dir_popup = if self.overlays.dir_popup.is_open() {
                DirectoryPopup::Hidden
            } else {
                DirectoryPopup::Manual
            };
            self.sync_view_timer(cx);
            self.reconcile_input_focus(window, cx);
            self.bump_activity(cx);
            self.notify_regions(
                DirtyRegions {
                    root: true,
                    viewport: true,
                    dock: true,
                    ..DirtyRegions::NONE
                },
                cx,
            );
            return;
        }
        let (dir_open, wb) = self.app.toggle_docked_directory();
        self.sync_view_timer(cx);
        self.reconcile_input_focus(window, cx);
        if dir_open {
            self.scroll_directory_to_current();
        } else {
            self.show_toast("侧栏已收起", cx);
        }
        let regions = self.apply_workbench_changes(wb, Some(window), cx);
        self.notify_regions(regions, cx);
    }

    pub(super) fn toggle_search(
        &mut self,
        _: &ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        let mut regions = DirtyRegions {
            root: true,
            sidebar: true,
            viewport: true,
            dock: true,
            ..DirtyRegions::NONE
        };
        if self.chrome.fullscreen {
            self.overlays.dir_popup = DirectoryPopup::Manual;
            self.bump_activity(cx);
        } else if let Some(wb) = self.app.ensure_docked_directory_open() {
            regions = regions.union(self.apply_workbench_changes(wb, Some(window), cx));
        }
        self.app.set_sidebar_mode(SidebarMode::Files);
        self.sync_view_timer(cx);
        self.text_inputs.search_open = !self.text_inputs.search_open;
        if self.text_inputs.search_open {
            self.text_inputs.search_focus.focus(window);
        } else {
            self.text_inputs.search_input.clear();
            self.filter_directory_rows();
            self.focus.focus(window);
        }
        self.notify_regions(regions, cx);
    }

    pub(super) fn refresh_directory(
        &mut self,
        _: &RefreshDirectory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_active_modal() {
            return;
        }
        if let ActiveSource::Plugin(ps) = self.catalog().active_source() {
            let (pid, sid) = (ps.plugin_id.clone(), ps.source_id.clone());
            self.refresh_plugin_source(&pid, &sid, cx);
            return;
        }
        let Some((ticket, load_generation)) = self.app.begin_local_directory_refresh() else {
            return;
        };
        let ticket_scan = ticket.clone();
        let task = cx
            .background_executor()
            .spawn(async move { ticket_scan.run_scan() });
        self.catalog_ui.directory_refresh = Some(cx.spawn(async move |this, cx| {
            let scan = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .app
                    .finish_local_directory_refresh(&ticket, scan, load_generation)
                {
                    return;
                }
                this.catalog_ui.directory_refresh = None;
                this.scroll_directory_to_current();
                this.ensure_filmstrip_thumbnails(cx);
                this.notify_regions(
                    DirtyRegions {
                        sidebar: true,
                        viewport: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
                this.show_toast("目录已刷新", cx);
            });
        }));
    }

    pub(super) fn open_settings(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_host_consent()
            || self.plugins().active_modal().is_some()
            || self.overlays.dialog.is_some()
        {
            return;
        }
        self.overlays.toggle_settings();
        self.focus.focus(window);
        self.reconcile_input_focus(window, cx);
        self.bump_activity(cx);
        self.notify_regions(
            DirtyRegions {
                root: true,
                settings: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
    }

    pub(super) fn open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_host_consent()
            || self.plugins().active_modal().is_some()
            || self.overlays.dialog.is_some()
        {
            return;
        }
        if self.overlays.toggle_palette() {
            self.text_inputs.palette_input.clear();
            self.overlays.palette_index = 0;
            self.text_inputs.palette_focus.focus(window);
        } else {
            self.focus.focus(window);
        }
        self.bump_activity(cx);
        self.notify_regions(
            DirtyRegions {
                root: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
    }

    pub(super) fn toggle_fullscreen(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.toggle_fullscreen();
    }

    pub(super) fn exit_fullscreen(
        &mut self,
        _: &ExitFullscreen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_escape(window, cx);
    }

    /// Single unified `Esc` dismissal pipeline across workspace, modals, menus, and text inputs.
    pub(in crate::ui) fn handle_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_host_consent() {
            self.cancel_host_consent(window, cx);
            return;
        }
        let has_plugin_modal = self.plugins().active_modal().is_some();
        let step = self.overlays.dismiss_next_escape(
            &mut self.text_inputs,
            has_plugin_modal,
            window.is_fullscreen(),
        );
        match step {
            EscapeStep::CancelRecording => {
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        settings: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::CloseContextMenu => {
                self.bump_activity(cx);
                self.notify_root(cx);
            }
            EscapeStep::CloseInfo => {
                self.bump_activity(cx);
                self.notify_viewport_and_dock(cx);
            }
            EscapeStep::CancelPluginModal => {
                self.cancel_active_plugin_modal(Some(window), cx);
            }
            EscapeStep::CloseBuiltinDialog
            | EscapeStep::ClosePalette
            | EscapeStep::ClosePopover => {
                self.sync_view_timer(cx);
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::CloseSettings => {
                self.sync_view_timer(cx);
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        settings: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::CloseDirPopup => {
                self.sync_view_timer(cx);
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        viewport: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::BlurSidebarForm => {
                self.sync_view_timer(cx);
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        plugin_panel: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::ClearSearchText => {
                self.filter_directory_rows();
                self.bump_activity(cx);
                self.notify_sidebar(cx);
            }
            EscapeStep::CloseSearch => {
                self.filter_directory_rows();
                self.focus.focus(window);
                self.bump_activity(cx);
                self.notify_regions(
                    DirtyRegions {
                        root: true,
                        sidebar: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }
            EscapeStep::ExitFullscreen => window.toggle_fullscreen(),
            EscapeStep::Noop => {}
        }
    }

    pub(super) fn toggle_image_size(
        &mut self,
        _: &ToggleImageSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.image_geometry_ready() {
            return;
        }
        let (viewport, media) = self.image_geometry();
        self.active_mut()
            .image
            .toggle_actual_size(viewport, media, window.scale_factor() as f64);
        self.apply_image_view(cx);
    }

    pub(super) fn toggle_image_size_at(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.image_geometry_ready() {
            return;
        }
        if self.active().image.zoomed() {
            self.active_mut().image.reset();
            self.apply_image_view(cx);
        } else {
            let (viewport, media) = self.image_geometry();
            let cur_scale =
                self.active()
                    .image
                    .pixel_scale(viewport, media, window.scale_factor() as f64);
            self.zoom_at((1. / cur_scale).log2(), Some(point), cx);
        }
    }

    pub(super) fn fit_image(&mut self, _: &FitImage, _: &mut Window, cx: &mut Context<Self>) {
        self.active_mut().image.reset();
        self.apply_image_view(cx);
    }

    pub(super) fn rotate_cw(&mut self, _: &RotateCw, _: &mut Window, cx: &mut Context<Self>) {
        self.rotate_media(90, "顺时针旋转 90°", cx);
    }

    pub(super) fn rotate_ccw(&mut self, _: &RotateCcw, _: &mut Window, cx: &mut Context<Self>) {
        self.rotate_media(-90, "逆时针旋转 90°", cx);
    }

    fn rotate_media(&mut self, degrees: i64, label: &'static str, cx: &mut Context<Self>) {
        let out = self.app.rotate_media(degrees, label);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_hflip(&mut self, _: &ToggleHFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.flip_display_axis(true, cx);
    }

    pub(super) fn toggle_vflip(&mut self, _: &ToggleVFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.flip_display_axis(false, cx);
    }

    fn flip_display_axis(&mut self, horizontal: bool, cx: &mut Context<Self>) {
        let out = self.app.flip_display_axis(horizontal);
        self.apply_playback_action(out, cx);
    }

    pub(super) fn toggle_filmstrip(
        &mut self,
        _: &ToggleFilmstrip,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (_, wb) = self.app.toggle_photo_filmstrip();
        self.ensure_filmstrip_thumbnails(cx);
        let regions = self.apply_workbench_changes(wb, None, cx);
        self.notify_regions(regions, cx);
    }

    pub(super) fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(0.25, None, cx);
    }

    pub(super) fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(-0.25, None, cx);
    }

    pub(super) fn open_file(&mut self, _: &OpenFile, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_active_modal() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("打开".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| this.open_paths(paths, cx));
            }
        })
        .detach();
    }
}
