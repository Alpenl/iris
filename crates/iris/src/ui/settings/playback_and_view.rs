//! Playback, subtitle & audio output devices, and photo viewer settings tabs.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use iris_core::player::AudioDevice;
use iris_core::settings::{Hwdec, OnEof, PhotoDefault, SubLang, VideoWheel};

use super::super::WindowShell;
use super::super::actions::ToggleFilmstrip;
use super::super::kit::form::{segmented, setting_row, stepper, toggle};
use super::super::kit::icon;
use super::super::views::model_listener;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

pub(super) fn settings_playback(
    model: &Entity<WindowShell>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let s = model.read(cx).settings();
    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(setting_row(
            "播完后行为",
            "视频播放到片尾时的动作",
            segmented(
                "set-eof",
                &[
                    ("停在末帧", OnEof::Pause),
                    ("播放下一项", OnEof::Next),
                    ("单曲循环", OnEof::Loop),
                ],
                s.on_eof,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_on_eof(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "方向键跳转步长",
            "快退快进单步秒数（大幅跳转为 6 倍）",
            segmented(
                "set-seek-step",
                &[("3s", 3), ("5s", 5), ("10s", 10), ("15s", 15), ("30s", 30)],
                s.seek_step.round() as i32,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_seek_step(val as f64);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "单击画面暂停 / 播放",
            "左键单击视频画面切换播放状态，双击仍为全屏",
            toggle(
                "set-click-pause",
                s.single_click_pause,
                t,
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.toggle_single_click_pause();
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
            ),
            t,
        ))
        .child(setting_row(
            "硬件解码",
            "遇到异常片源花屏时可切换为软解",
            segmented(
                "set-hwdec",
                &[("自动安全", Hwdec::AutoSafe), ("软解", Hwdec::Off)],
                s.hwdec,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_hwdec(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "记住播放进度",
            "再次打开同一本地或远程视频时从上次位置续播",
            toggle(
                "set-resume",
                s.resume_playback,
                t,
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.toggle_resume_playback();
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
            ),
            t,
        ))
        .child(setting_row(
            "启动时恢复倍速",
            "关闭后每次启动恢复为 1.00× 常速",
            toggle(
                "set-speed",
                s.remember_speed,
                t,
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.toggle_remember_speed();
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
            ),
            t,
        ))
        .child(setting_row(
            "截图包含字幕",
            "关闭后截图仅保存原始视频帧画面",
            toggle(
                "set-shot-subs",
                s.screenshot_subs,
                t,
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.toggle_screenshot_subs();
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
            ),
            t,
        ))
}

pub(super) fn settings_tracks(
    model: &Entity<WindowShell>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let s = m.settings();
    let mut devices = m.session().audio_devices();
    if !devices.iter().any(|d| d.name == "auto") {
        devices.insert(
            0,
            AudioDevice {
                name: "auto".into(),
                description: String::new(),
            },
        );
    }
    let cur_dev = if s.audio_device.trim().is_empty() {
        "auto"
    } else {
        s.audio_device.as_str()
    };

    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(setting_row(
            "字幕优先语言",
            "多字幕轨视频自动选择的语言顺序",
            segmented(
                "set-slang",
                &[
                    ("简中", SubLang::ZhHans),
                    ("繁中", SubLang::ZhHant),
                    ("英文", SubLang::En),
                    ("自动", SubLang::Auto),
                ],
                s.sub_lang,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_sub_lang(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "字幕字号缩放",
            "按比例放大或缩小视频字幕",
            stepper(
                "set-sub-scale",
                format!("{:.2}×", s.sub_scale),
                (s.sub_scale - 1.0).abs() > 0.01,
                t,
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.step_sub_scale(-0.1);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.step_sub_scale(0.1);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
                model_listener(model, |this, _, _, cx| {
                    let wb = this.app.reset_sub_scale();
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                }),
            ),
            t,
        ))
        .child(setting_row(
            "视频画面滚轮行为",
            "指针在视频画面上方滚动滚轮时的响应",
            segmented(
                "set-wheel",
                &[
                    ("调节音量", VideoWheel::Volume),
                    ("无操作", VideoWheel::None),
                ],
                s.video_wheel,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_video_wheel(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(t.text_md)
                                .text_color(t.text)
                                .child("默认音频输出设备"),
                        )
                        .child(
                            div()
                                .text_size(t.text_sm)
                                .text_color(t.text_faint)
                                .child("选择音频输出通道，切换立即生效并持久化"),
                        ),
                )
                .child(div().flex().flex_col().gap(px(4.)).children(
                    devices.into_iter().enumerate().map(|(di, dev)| {
                        let active = dev.name == cur_dev;
                        let dev_name = dev.name.clone();
                        let desc_toast =
                            super::super::strings::chrome::audio_device_label(&dev, false);
                        let label_txt =
                            super::super::strings::chrome::audio_device_label(&dev, true);
                        div()
                            .id(("audio-dev-row", di))
                            .px(px(10.))
                            .py(px(7.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .rounded(t.radius_md)
                            .cursor_pointer()
                            .bg(if active { t.selected } else { t.window_bg })
                            .border_1()
                            .border_color(if active { t.accent } else { t.line })
                            .hover(|st| st.bg(t.hover))
                            .on_click(model_listener(model, move |this, _, _, cx| {
                                let (playback_out, wb) =
                                    this.app.set_audio_device(dev_name.clone());
                                this.apply_playback_action(playback_out, cx);
                                let regions = this.apply_workbench_changes(wb, None, cx);
                                this.notify_regions(regions, cx);
                                this.show_toast(format!("音频输出 · {desc_toast}"), cx);
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.))
                                    .child(
                                        div()
                                            .text_size(t.text_md)
                                            .text_color(if active { t.text } else { t.text_muted })
                                            .truncate()
                                            .child(label_txt),
                                    )
                                    .when(dev.name != "auto", |d| {
                                        d.child(
                                            div()
                                                .font_family(FONT_MONO)
                                                .text_size(t.text_xs)
                                                .text_color(t.text_faint)
                                                .truncate()
                                                .child(dev.name),
                                        )
                                    }),
                            )
                            .when(active, |d| d.child(icon(Icon::Check, 13., t.accent)))
                    }),
                )),
        )
}

pub(super) fn settings_view(
    model: &Entity<WindowShell>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let s = model.read(cx).settings();
    div()
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(setting_row(
            "全屏界面隐藏时间",
            "鼠标静止多久后自动隐藏全屏工具栏与光标",
            segmented(
                "set-hide-ms",
                &[("1.5s", 1500), ("2s", 2000), ("3s", 3000), ("5s", 5000)],
                s.fullscreen_hide_ms,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_fullscreen_hide_ms(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "照片默认视图",
            "打开新照片时的初始缩放模式",
            segmented(
                "set-photo-default",
                &[
                    ("适应窗口", PhotoDefault::Fit),
                    ("原始 1:1", PhotoDefault::Actual),
                ],
                s.photo_default,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_photo_default(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "看图双击行为",
            "双击照片时切换原始大小 1:1 或切换全屏",
            segmented(
                "set-photo-dbl",
                &[("切换 1:1", true), ("切换全屏", false)],
                s.photo_double_click_actual,
                t,
                model,
                |this, val, _, cx| {
                    let wb = this.app.set_photo_double_click_actual(val);
                    let regions = this.apply_workbench_changes(wb, None, cx);
                    this.notify_regions(regions, cx);
                },
            ),
            t,
        ))
        .child(setting_row(
            "照片底部缩略图条",
            "浏览多张照片时在画面下方显示内存缩略图快速跳转",
            toggle(
                "set-filmstrip",
                s.photo_filmstrip,
                t,
                model_listener(model, |this, _, w, cx| {
                    this.toggle_filmstrip(&ToggleFilmstrip, w, cx);
                }),
            ),
            t,
        ))
}
