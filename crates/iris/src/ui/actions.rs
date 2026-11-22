//! GPUI action declarations, context-scoped keybindings, and action handlers.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, Context, KeyBinding, PathPromptOptions, Pixels, Point, Window, actions};

use super::*;
use crate::library::{self, Kind};
use crate::platform;
use crate::player::{self, Command, TrackKind};

actions!(
    iris,
    [
        TogglePause,
        SeekBackwardExact,
        SeekForwardExact,
        SeekBackwardLarge,
        SeekForwardLarge,
        FrameBack,
        FrameForward,
        VolumeUp,
        VolumeDown,
        ToggleMute,
        SpeedDown,
        SpeedUp,
        ResetSpeed,
        ToggleLoop,
        ToggleDirectory,
        ToggleSearch,
        RefreshDirectory,
        ToggleFullscreen,
        ExitFullscreen,
        OpenSettings,
        OpenCommandPalette,
        ToggleAlwaysOnTop,
        ToggleInfo,
        TakeScreenshot,
        CycleAspect,
        ToggleSubs,
        CycleSub,
        SubDelayBack,
        SubDelayForward,
        SubDelayReset,
        CycleAudio,
        PrevChapter,
        NextChapter,
        ToggleImageSize,
        FitImage,
        ZoomIn,
        ZoomOut,
        RotateCw,
        RotateCcw,
        ToggleHFlip,
        ToggleVFlip,
        ToggleFilmstrip,
        OpenNextMedia,
        OpenPreviousMedia,
        OpenFile,
        ToggleFavorite,
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Video & audio transport
        KeyBinding::new("space", TogglePause, Some("Viewport && media == video")),
        KeyBinding::new("space", TogglePause, Some("Viewport && media == audio")),
        KeyBinding::new(
            "left",
            SeekBackwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "left",
            SeekBackwardExact,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "right",
            SeekForwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "right",
            SeekForwardExact,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "shift-left",
            SeekBackwardLarge,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "shift-left",
            SeekBackwardLarge,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "shift-right",
            SeekForwardLarge,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "shift-right",
            SeekForwardLarge,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(",", FrameBack, Some("Viewport && media == video")),
        KeyBinding::new(".", FrameForward, Some("Viewport && media == video")),
        KeyBinding::new("up", VolumeUp, Some("Viewport && media == video")),
        KeyBinding::new("up", VolumeUp, Some("Viewport && media == audio")),
        KeyBinding::new("down", VolumeDown, Some("Viewport && media == video")),
        KeyBinding::new("down", VolumeDown, Some("Viewport && media == audio")),
        KeyBinding::new("m", ToggleMute, Some("Viewport && media == video")),
        KeyBinding::new("m", ToggleMute, Some("Viewport && media == audio")),
        KeyBinding::new("[", SpeedDown, Some("Viewport && media == video")),
        KeyBinding::new("[", SpeedDown, Some("Viewport && media == audio")),
        KeyBinding::new("]", SpeedUp, Some("Viewport && media == video")),
        KeyBinding::new("]", SpeedUp, Some("Viewport && media == audio")),
        KeyBinding::new("backspace", ResetSpeed, Some("Viewport && media == video")),
        KeyBinding::new("backspace", ResetSpeed, Some("Viewport && media == audio")),
        KeyBinding::new("l", ToggleLoop, Some("Viewport && media == video")),
        KeyBinding::new("l", ToggleLoop, Some("Viewport && media == audio")),
        KeyBinding::new("v", ToggleSubs, Some("Viewport && media == video")),
        KeyBinding::new("j", CycleSub, Some("Viewport && media == video")),
        KeyBinding::new("z", SubDelayBack, Some("Viewport && media == video")),
        KeyBinding::new("x", SubDelayForward, Some("Viewport && media == video")),
        KeyBinding::new("shift-z", SubDelayReset, Some("Viewport && media == video")),
        KeyBinding::new("a", CycleAudio, Some("Viewport && media == video")),
        KeyBinding::new("c", CycleAspect, Some("Viewport && media == video")),
        KeyBinding::new("shift-[", PrevChapter, Some("Viewport && media == video")),
        KeyBinding::new("shift-]", NextChapter, Some("Viewport && media == video")),
        KeyBinding::new("r", RotateCw, Some("Viewport && media == video")),
        KeyBinding::new("shift-r", RotateCcw, Some("Viewport && media == video")),
        KeyBinding::new("h", ToggleHFlip, Some("Viewport && media == video")),
        KeyBinding::new("shift-h", ToggleVFlip, Some("Viewport && media == video")),
        // Photos
        KeyBinding::new("space", ToggleImageSize, Some("Viewport && media == image")),
        KeyBinding::new("0", FitImage, Some("Viewport && media == image")),
        KeyBinding::new("=", ZoomIn, Some("Viewport && media == image")),
        KeyBinding::new("-", ZoomOut, Some("Viewport && media == image")),
        KeyBinding::new("r", RotateCw, Some("Viewport && media == image")),
        KeyBinding::new("shift-r", RotateCcw, Some("Viewport && media == image")),
        KeyBinding::new("h", ToggleHFlip, Some("Viewport && media == image")),
        KeyBinding::new("shift-h", ToggleVFlip, Some("Viewport && media == image")),
        KeyBinding::new("g", ToggleFilmstrip, Some("Viewport && media == image")),
        KeyBinding::new(
            "left",
            OpenPreviousMedia,
            Some("Viewport && media == image"),
        ),
        KeyBinding::new("right", OpenNextMedia, Some("Viewport && media == image")),
        // Workspace / general (single-key shortcuts disabled while typing in text inputs)
        KeyBinding::new("tab", ToggleDirectory, Some("Workspace && !TextInput")),
        KeyBinding::new("f", ToggleFullscreen, Some("Workspace && !TextInput")),
        KeyBinding::new("t", ToggleAlwaysOnTop, Some("Workspace && !TextInput")),
        KeyBinding::new("i", ToggleInfo, Some("Viewport")),
        KeyBinding::new("secondary-i", ToggleInfo, Some("Workspace")),
        KeyBinding::new("secondary-s", TakeScreenshot, Some("Workspace")),
        KeyBinding::new("secondary-f", ToggleSearch, Some("Workspace")),
        KeyBinding::new("escape", ExitFullscreen, Some("Workspace && !TextInput")),
        KeyBinding::new("pagedown", OpenNextMedia, Some("Viewport")),
        KeyBinding::new("pageup", OpenPreviousMedia, Some("Viewport")),
        KeyBinding::new("s", ToggleFavorite, Some("Viewport")),
        KeyBinding::new("secondary-o", OpenFile, Some("Workspace")),
        KeyBinding::new("secondary-r", RefreshDirectory, Some("Workspace")),
        KeyBinding::new("secondary-p", OpenCommandPalette, Some("Workspace")),
        KeyBinding::new("secondary-shift-p", OpenCommandPalette, Some("Workspace")),
        KeyBinding::new("secondary-,", OpenSettings, Some("Workspace")),
        KeyBinding::new("secondary-q", Quit, Some("Workspace")),
    ]);
}

/// Formats a UTC timestamp (`YYYYMMDD-HHMMSS-mmm`) without external crates.
fn format_screenshot_stamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs() as i64;
    let ms = dur.subsec_millis();
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let min = (secs_of_day % 3600) / 60;
    let sec = secs_of_day % 60;
    // Civil date from days since 1970-01-01 (Howard Hinnant algorithm).
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
        let Some(path) = self.active().session.current.clone() else {
            return;
        };
        let added = self.store.toggle_favorite(&path);
        self.rebuild_directory_rows();
        self.show_toast(
            if added {
                "已收藏 · S 取消"
            } else {
                "已取消收藏"
            },
            cx,
        );
    }

    pub(super) fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        let state = &self.active().session.state;
        let next_paused = !state.paused && !state.ended;
        self.command(Command::TogglePlayback, cx);
        self.show_osd(if next_paused { "暂停" } else { "播放" }, cx);
    }

    pub(super) fn seek_back(
        &mut self,
        _: &SeekBackwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self.settings.seek_step;
        let target = (self.active().session.state.time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(format!("-{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_forward(
        &mut self,
        _: &SeekForwardExact,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self.settings.seek_step;
        let state = &self.active().session.state;
        let target = (state.time_pos + step).min(state.duration.max(0.));
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(format!("+{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_back_large(
        &mut self,
        _: &SeekBackwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let target = (self.active().session.state.time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(format!("-{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn seek_forward_large(
        &mut self,
        _: &SeekForwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let state = &self.active().session.state;
        let target = (state.time_pos + step).min(state.duration.max(0.));
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(format!("+{step:.0}s · {}", player::format_time(target)), cx);
    }

    pub(super) fn frame_back(&mut self, _: &FrameBack, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::FrameBackStep, cx);
        self.show_osd("上一帧", cx);
    }

    pub(super) fn frame_forward(
        &mut self,
        _: &FrameForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command(Command::FrameStep, cx);
        self.show_osd("下一帧", cx);
    }

    pub(super) fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active().session.state.volume + 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }

    pub(super) fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active().session.state.volume - 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }

    pub(super) fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state.muted;
        self.command(Command::ToggleMute, cx);
        self.show_osd(if next { "已静音" } else { "已取消静音" }, cx);
    }

    pub(super) fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.active().session.state.speed, -1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }

    pub(super) fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.active().session.state.speed, 1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }

    pub(super) fn reset_speed(&mut self, _: &ResetSpeed, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SetSpeed(1.), cx);
        self.show_osd("倍速 · 1.00×", cx);
    }

    pub(super) fn toggle_loop(&mut self, _: &ToggleLoop, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state.loop_file;
        self.command(Command::SetLoop(next), cx);
        self.show_osd(
            if next {
                "单曲循环已开启"
            } else {
                "单曲循环已关闭"
            },
            cx,
        );
    }

    pub(super) fn toggle_always_on_top(
        &mut self,
        _: &ToggleAlwaysOnTop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.always_on_top = !self.settings.always_on_top;
        platform::set_always_on_top(window, self.settings.always_on_top);
        self.settings.save();
        self.show_toast(
            if self.settings.always_on_top {
                "窗口已置顶 · T 取消"
            } else {
                "已取消窗口置顶"
            },
            cx,
        );
    }

    pub(super) fn toggle_info(&mut self, _: &ToggleInfo, _: &mut Window, cx: &mut Context<Self>) {
        self.overlays.info_open = !self.overlays.info_open;
        if self.overlays.info_open {
            self.refresh_cached_stats(true);
        }
        cx.notify();
    }

    pub(super) fn take_screenshot(
        &mut self,
        _: &TakeScreenshot,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active().session.kind() == Some(Kind::Audio) {
            self.show_toast("当前为音频文件，无法截图", cx);
            return;
        }
        let Some(cur) = self.active().session.current.as_ref() else {
            return;
        };
        let stem = cur
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "iris".into());
        let stamp = format_screenshot_stamp();
        let dir = platform::default_screenshot_dir();
        let out = dir.join(format!("Iris-{stem}-{stamp}.png"));
        let with_subs = self.settings.screenshot_subs;
        if let Err(e) = self.active_mut().session.execute(Command::Screenshot {
            path: out.clone(),
            with_subs,
        }) {
            self.show_toast(format!("截图失败 · {e}"), cx);
            return;
        }
        self.show_toast(
            format!(
                "已请求截图 · {}",
                out.file_name().unwrap_or_default().to_string_lossy()
            ),
            cx,
        );
    }

    pub(super) fn cycle_aspect(&mut self, _: &CycleAspect, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.active().session.state.aspect.next();
        self.command(Command::SetAspect(next), cx);
        self.show_osd(format!("画面比例 · {}", next.label()), cx);
    }

    pub(super) fn toggle_directory(
        &mut self,
        _: &ToggleDirectory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.close_menus();
        if self.fullscreen {
            self.overlays.dir_popup = !self.overlays.dir_popup;
            self.bump_activity(cx);
            cx.notify();
            return;
        }
        self.settings.directory_open = !self.settings.directory_open;
        self.settings.save();
        if self.settings.directory_open {
            self.scroll_directory_to_current();
            cx.notify();
        } else {
            self.show_toast("目录已收起 · Tab 展开", cx);
        }
    }

    pub(super) fn toggle_search(
        &mut self,
        _: &ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.fullscreen {
            self.overlays.dir_popup = true;
            self.bump_activity(cx);
        } else if !self.settings.directory_open {
            self.settings.directory_open = true;
        }
        self.sidebar_plugins = false;
        self.search_open = !self.search_open;
        if self.search_open {
            self.search_focus.focus(window);
        } else {
            self.search_input.clear();
            self.rebuild_directory_rows();
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
        let Some(folder) = self.active().session.folder.clone() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { library::scan_dir(&folder) });
        cx.spawn(async move |this, cx| {
            let scan = task.await;
            let _ = this.update(cx, |this, cx| {
                this.active_mut().session.entries = scan.entries;
                this.active_mut().session.subfolders = scan.subfolders;
                this.rebuild_directory_rows();
                this.scroll_directory_to_current();
                this.show_toast("目录已刷新", cx);
            });
        })
        .detach();
    }

    pub(super) fn open_settings(
        &mut self,
        _: &OpenSettings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.toggle_settings();
        self.bump_activity(cx);
        cx.notify();
    }

    pub(super) fn open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.toggle_palette() {
            self.palette_input.clear();
            self.palette_index = 0;
            self.palette_focus.focus(window);
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
        if self.overlays.context_menu.is_some() {
            self.overlays.context_menu = None;
            cx.notify();
        } else if self.overlays.palette_open {
            self.overlays.palette_open = false;
            self.palette_input.clear();
            self.focus.focus(window);
            self.bump_activity(cx);
            cx.notify();
        } else if self.overlays.settings_open {
            self.overlays.settings_open = false;
            self.bump_activity(cx);
            cx.notify();
        } else if self.overlays.popover.is_some() {
            self.overlays.popover = None;
            self.bump_activity(cx);
            cx.notify();
        } else if self.overlays.info_open {
            self.overlays.info_open = false;
            cx.notify();
        } else if self.search_open {
            self.search_open = false;
            self.search_input.clear();
            self.rebuild_directory_rows();
            self.focus.focus(window);
            cx.notify();
        } else if self.overlays.dir_popup {
            self.overlays.dir_popup = false;
            self.bump_activity(cx);
            cx.notify();
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }

    pub(super) fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.active().session.state.subs_visible;
        self.command(Command::ToggleSubtitles, cx);
        self.show_osd(
            if next {
                "字幕已显示"
            } else {
                "字幕已隐藏"
            },
            cx,
        );
    }

    pub(super) fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.tracks_of(TrackKind::Sub).is_empty() {
            self.show_osd("当前视频没有字幕轨", cx);
            return;
        }
        self.announce_sub = true;
        self.command(Command::NextSubtitle, cx);
    }

    pub(super) fn sub_delay_back(
        &mut self,
        _: &SubDelayBack,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = ((self.active().session.state.sub_delay - 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }

    pub(super) fn sub_delay_forward(
        &mut self,
        _: &SubDelayForward,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = ((self.active().session.state.sub_delay + 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }

    pub(super) fn sub_delay_reset(
        &mut self,
        _: &SubDelayReset,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command(Command::SetSubDelay(0.), cx);
        self.show_osd("字幕偏移 · 0.0s", cx);
    }

    pub(super) fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.tracks_of(TrackKind::Audio).len() <= 1 {
            self.show_osd("当前视频没有其他音轨", cx);
            return;
        }
        self.announce_audio = true;
        self.command(Command::NextAudio, cx);
    }

    pub(super) fn prev_chapter(&mut self, _: &PrevChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.state.chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::PrevChapter, cx);
        self.show_osd("上一章节", cx);
    }

    pub(super) fn next_chapter(&mut self, _: &NextChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.active().session.state.chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::NextChapter, cx);
        self.show_osd("下一章节", cx);
    }

    pub(super) fn toggle_image_size(
        &mut self,
        _: &ToggleImageSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if self.active().image.zoomed() {
            self.active_mut().image.reset();
            self.apply_image_view(cx);
        } else {
            let (viewport, media) = self.image_geometry();
            let scale = window.scale_factor() as f64;
            let current_scale = self
                .active()
                .image
                .pixel_scale(viewport, media, scale)
                .max(0.0001);
            let delta = (1. / current_scale).log2();
            self.zoom_at(delta, Some(point), cx);
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
        self.command(Command::ToggleHFlip, cx);
        let on = self.active().session.state.hflip;
        self.show_osd(
            if on {
                "水平镜像"
            } else {
                "还原水平镜像"
            },
            cx,
        );
    }

    pub(super) fn toggle_vflip(&mut self, _: &ToggleVFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::ToggleVFlip, cx);
        let on = self.active().session.state.vflip;
        self.show_osd(
            if on {
                "垂直镜像"
            } else {
                "还原垂直镜像"
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
        self.settings.save();
        if self.settings.photo_filmstrip {
            self.ensure_filmstrip_thumbnails(cx);
        }
        cx.notify();
    }

    pub(super) fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(0.25, None, cx);
    }

    pub(super) fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(-0.25, None, cx);
    }

    pub(super) fn next_item(&mut self, _: &OpenNextMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    pub(super) fn prev_item(
        &mut self,
        _: &OpenPreviousMedia,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(-1, cx);
    }

    pub(super) fn open_file(&mut self, _: &OpenFile, _: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("打开".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.open_path(&p, cx));
            }
        })
        .detach();
    }
}
