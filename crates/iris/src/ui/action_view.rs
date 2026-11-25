//! Window, preset, sidebar, overlay, screenshot, and photo-view action handlers.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{Context, PathPromptOptions, Pixels, Point, Window};
use iris_core::library::{self, Kind};
use iris_core::player::Command;
use iris_core::preset::SidebarMode;

use super::IrisWindow;
use super::actions::{
    ActivatePreset, CycleAspect, CyclePreset, ExitFullscreen, FitImage, OpenCommandPalette,
    OpenFile, OpenSettings, OpenStreamUrl, RefreshDirectory, RotateCcw, RotateCw, TakeScreenshot,
    ToggleAlwaysOnTop, ToggleDirectory, ToggleFavorite, ToggleFilmstrip, ToggleFullscreen,
    ToggleHFlip, ToggleImageSize, ToggleInfo, ToggleSearch, ToggleVFlip, TriggerPluginCommand,
    ZoomIn, ZoomOut,
};
use super::chrome::DirectoryPopup;
use super::state::{ActiveSource, BuiltinDialog, EscapeStep};
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

impl IrisWindow {
    pub(super) fn toggle_favorite(
        &mut self,
        _: &ToggleFavorite,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(uri) = self.active().session.current_media_uri() else {
            return;
        };
        if !self.active().session.may_share_current_identity() {
            self.show_toast("此链接仅用于本次播放，无法收藏", cx);
            return;
        }
        let added = self.store.toggle_favorite(&uri);
        self.rebuild_directory_rows();
        self.scan_visible_favorites(cx);
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
        let next = self
            .presets
            .next_id(&self.settings.active_preset)
            .to_string();
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
        self.settings.always_on_top = !self.settings.always_on_top;
        platform::set_always_on_top(window, self.settings.always_on_top);
        self.persist_user_settings();
        self.show_toast(
            if self.settings.always_on_top {
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
            self.active_mut().refresh_cached_stats(true);
            self.refresh_annotations(cx);
        }
        cx.notify();
    }

    pub(super) fn take_screenshot(
        &mut self,
        _: &TakeScreenshot,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = &self.active().session;
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
        let with_subs = self.settings.screenshot_subs;
        if let Err(e) = self.active_mut().session.execute(Command::Screenshot {
            path: out.clone(),
            with_subs,
        }) {
            self.show_toast(format!("截图失败 · {e}"), cx);
            return;
        }
        let name = out.file_name().unwrap_or_default().to_string_lossy();
        self.show_toast(format!("已请求截图 · {name}"), cx);
    }

    pub(super) fn cycle_aspect(&mut self, _: &CycleAspect, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.active().session.state().aspect.next();
        self.command(Command::SetAspect(next), cx);
        self.show_osd(format!("画面比例 · {}", next.label()), cx);
    }

    pub(super) fn toggle_directory(
        &mut self,
        _: &ToggleDirectory,
        _: &mut Window,
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
            self.bump_activity(cx);
            cx.notify();
            return;
        }
        self.settings.directory_open = !self.settings.directory_open;
        self.persist_user_settings();
        if self.settings.directory_open {
            self.scroll_directory_to_current();
            cx.notify();
        } else {
            self.show_toast("侧栏已收起", cx);
        }
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
        if self.chrome.fullscreen {
            self.overlays.dir_popup = DirectoryPopup::Manual;
            self.bump_activity(cx);
        } else if !self.settings.directory_open {
            self.settings.directory_open = true;
            self.persist_user_settings();
        }
        self.catalog.sidebar_mode = SidebarMode::Files;
        self.text_inputs.search_open = !self.text_inputs.search_open;
        if self.text_inputs.search_open {
            self.text_inputs.search_focus.focus(window);
        } else {
            self.text_inputs.search_input.clear();
            self.filter_directory_rows();
            self.focus.focus(window);
        }
        cx.notify();
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
        if let ActiveSource::Plugin(ps) = &self.catalog.active_source {
            let (pid, sid, fid) = (
                ps.plugin_id.clone(),
                ps.source_id.clone(),
                ps.folder_id.clone(),
            );
            let q = (!self.text_inputs.search_input.is_empty())
                .then(|| self.text_inputs.search_input.text().trim().to_string())
                .filter(|s| !s.is_empty());
            self.browse_plugin_source(&pid, &sid, fid, None, q, cx);
            return;
        }
        if self.active().pending_path.is_some() {
            return;
        }
        let Some(folder) = self.active().session.folder().map(Path::to_path_buf) else {
            return;
        };
        let generation = self.active().load_generation;
        let f_scan = folder.clone();
        let task = cx
            .background_executor()
            .spawn(async move { library::scan_local_or_share(&f_scan) });
        self.catalog.directory_refresh = Some(cx.spawn(async move |this, cx| {
            let scan = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active().load_generation != generation
                    || this.active().pending_path.is_some()
                    || !this.catalog.active_source.is_local()
                    || this.active().session.folder() != Some(folder.as_path())
                {
                    return;
                }
                this.active_mut().session.set_directory(
                    Some(folder),
                    scan.entries,
                    scan.subfolders,
                );
                this.rebuild_directory_rows();
                this.scroll_directory_to_current();
                this.ensure_filmstrip_thumbnails(cx);
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
            || self.plugins.active_modal().is_some()
            || self.overlays.dialog.is_some()
        {
            return;
        }
        self.overlays.toggle_settings();
        self.focus.focus(window);
        self.bump_activity(cx);
        cx.notify();
    }

    pub(super) fn open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_host_consent()
            || self.plugins.active_modal().is_some()
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
        cx.notify();
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
        if self.has_host_consent() {
            self.cancel_host_consent(window, cx);
            return;
        }
        let has_plugin_modal = self.plugins.active_modal().is_some();
        match self.overlays.dismiss_next_escape(
            &mut self.text_inputs,
            has_plugin_modal,
            window.is_fullscreen(),
        ) {
            EscapeStep::CancelRecording | EscapeStep::CloseContextMenu | EscapeStep::CloseInfo => {
                self.bump_activity(cx);
                cx.notify();
            }
            EscapeStep::CancelPluginModal => {
                self.cancel_active_plugin_modal(Some(window), cx);
            }
            EscapeStep::CloseBuiltinDialog => {
                self.focus.focus(window);
                self.bump_activity(cx);
                cx.notify();
            }
            EscapeStep::ClosePalette => {
                self.focus.focus(window);
                self.bump_activity(cx);
                cx.notify();
            }
            EscapeStep::CloseSettings | EscapeStep::ClosePopover | EscapeStep::CloseDirPopup => {
                self.focus.focus(window);
                self.bump_activity(cx);
                cx.notify();
            }
            EscapeStep::CloseSearch => {
                self.filter_directory_rows();
                self.focus.focus(window);
                self.bump_activity(cx);
                cx.notify();
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
        self.active_mut().image.reset();
        self.command(Command::RotateMedia(90), cx);
        self.apply_image_view(cx);
        self.show_osd("顺时针旋转 90°", cx);
    }

    pub(super) fn rotate_ccw(&mut self, _: &RotateCcw, _: &mut Window, cx: &mut Context<Self>) {
        self.active_mut().image.reset();
        self.command(Command::RotateMedia(-90), cx);
        self.apply_image_view(cx);
        self.show_osd("逆时针旋转 90°", cx);
    }

    pub(super) fn toggle_hflip(&mut self, _: &ToggleHFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.flip_display_axis(true, cx);
    }

    pub(super) fn toggle_vflip(&mut self, _: &ToggleVFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.flip_display_axis(false, cx);
    }

    fn flip_display_axis(&mut self, horizontal: bool, cx: &mut Context<Self>) {
        let command = if horizontal {
            Command::ToggleHFlip
        } else {
            Command::ToggleVFlip
        };
        if let Err(error) = self.active_mut().session.execute(command) {
            self.show_toast(format!("操作失败 · {error}"), cx);
            return;
        }
        self.active_mut().image.pan[usize::from(!horizontal)] *= -1.;
        self.apply_image_view(cx);
        let session = &self.active().session;
        let source_horizontal = horizontal ^ (session.display_rotation().rem_euclid(180) == 90);
        let state = session.state();
        let on = if source_horizontal {
            state.hflip
        } else {
            state.vflip
        };
        self.show_osd(
            match (horizontal, on) {
                (true, true) => "水平镜像",
                (true, false) => "还原水平镜像",
                (false, true) => "垂直镜像",
                (false, false) => "还原垂直镜像",
            },
            cx,
        );
    }

    pub(super) fn toggle_filmstrip(
        &mut self,
        _: &ToggleFilmstrip,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.photo_filmstrip = !self.settings.photo_filmstrip;
        self.persist_user_settings();
        self.ensure_filmstrip_thumbnails(cx);
        cx.notify();
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
