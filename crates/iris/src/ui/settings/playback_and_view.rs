//! Playback, subtitle & audio output devices, and photo viewer settings tabs.

use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, prelude::FluentBuilder, px,
};
use iris_core::player::{AudioDevice, Command};
use iris_core::settings::{Hwdec, OnEof, PhotoDefault, SubLang, VideoWheel};

use super::super::IrisWindow;
use super::super::actions::ToggleFilmstrip;
use super::super::kit::form::{segmented, setting_row, stepper, toggle};
use super::super::kit::icon;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

impl IrisWindow {
    pub(super) fn settings_playback(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = &self.settings;
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
                    cx,
                    |this, val, cx| {
                        this.settings.on_eof = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_on_eof_loop(val);
                        cx.notify();
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
                    cx,
                    |this, val, cx| {
                        this.settings.seek_step = val as f64;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx.listener(|this, _, _, cx| {
                        this.settings.single_click_pause = !this.settings.single_click_pause;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx,
                    |this, val, cx| {
                        this.settings.hwdec = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_hwdec(val);
                        cx.notify();
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
                    cx.listener(|this, _, _, cx| {
                        this.settings.resume_playback = !this.settings.resume_playback;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx.listener(|this, _, _, cx| {
                        this.settings.remember_speed = !this.settings.remember_speed;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx.listener(|this, _, _, cx| {
                        this.settings.screenshot_subs = !this.settings.screenshot_subs;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
    }

    pub(super) fn settings_tracks(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = &self.settings;
        let mut devices = self.active().session.audio_devices();
        if !devices.iter().any(|d| d.name == "auto") {
            devices.insert(
                0,
                AudioDevice {
                    name: "auto".into(),
                    description: "自动跟随系统默认输出设备".into(),
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
                    cx,
                    |this, val, cx| {
                        this.settings.sub_lang = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_sub_lang(val);
                        cx.notify();
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
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale =
                            (((this.settings.sub_scale - 0.1) * 100.).round() / 100.)
                                .clamp(0.5, 2.5);
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale =
                            (((this.settings.sub_scale + 0.1) * 100.).round() / 100.)
                                .clamp(0.5, 2.5);
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale = 1.0;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
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
                    cx,
                    |this, val, cx| {
                        this.settings.video_wheel = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                                    .text_size(px(12.))
                                    .text_color(t.text)
                                    .child("默认音频输出设备"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(t.text_faint)
                                    .child("选择音频输出通道，切换立即生效并持久化"),
                            ),
                    )
                    .child(div().flex().flex_col().gap(px(4.)).children(
                        devices.into_iter().enumerate().map(|(di, dev)| {
                            let active = dev.name == cur_dev;
                            let dev_name = dev.name.clone();
                            let desc_toast = if dev.description.trim().is_empty() {
                                dev.name.clone()
                            } else {
                                dev.description.clone()
                            };
                            let label_txt = if dev.name == "auto" {
                                "自动选择（跟随系统默认）".to_string()
                            } else if dev.description.trim().is_empty() {
                                dev.name.clone()
                            } else {
                                dev.description
                            };
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
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.settings.audio_device = dev_name.clone();
                                    this.preset_rt.record_user_edit(&this.settings);
                                    this.command(Command::SetAudioDevice(dev_name.clone()), cx);
                                    this.show_toast(format!("音频输出 · {desc_toast}"), cx);
                                    cx.notify();
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
                                                .text_size(px(12.))
                                                .text_color(if active {
                                                    t.text
                                                } else {
                                                    t.text_muted
                                                })
                                                .truncate()
                                                .child(label_txt),
                                        )
                                        .when(dev.name != "auto", |d| {
                                            d.child(
                                                div()
                                                    .font_family(FONT_MONO)
                                                    .text_size(px(10.))
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

    pub(super) fn settings_view(&self, t: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.settings;
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
                    cx,
                    |this, val, cx| {
                        this.settings.fullscreen_hide_ms = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx,
                    |this, val, cx| {
                        this.settings.photo_default = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx,
                    |this, val, cx| {
                        this.settings.photo_double_click_actual = val;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
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
                    cx.listener(|this, _, w, cx| {
                        this.toggle_filmstrip(&ToggleFilmstrip, w, cx);
                    }),
                ),
                t,
            ))
    }
}
