//! In-window centered modal settings panel (Direction C: 1px borders, sharp
//! corners, hand-written toggle / segmented / stepper controls).
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::widgets::{icon, icon_button, tip};
use super::*;
use crate::assets::Icon;
use crate::logging;
use crate::settings::{
    DEFAULT_DIRECTORY_WIDTH, Hwdec, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault,
    Side, SubLang, VideoWheel, path as settings_path,
};

impl IrisWindow {
    pub(super) fn render_settings_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.settings_tab {
            SettingsTab::Appearance => self.settings_appearance(cx).into_any_element(),
            SettingsTab::Playback => self.settings_playback(cx).into_any_element(),
            SettingsTab::Tracks => self.settings_tracks(cx).into_any_element(),
            SettingsTab::View => self.settings_view(cx).into_any_element(),
            SettingsTab::Shortcuts => self.settings_shortcuts(cx).into_any_element(),
        };

        let nav_item = |tab: SettingsTab, label: &'static str| {
            let active = self.settings_tab == tab;
            div()
                .id(("settings-tab", tab as usize))
                .relative()
                .h(px(32.))
                .px(px(14.))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_dim())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(6.))
                            .bottom(px(6.))
                            .w(px(2.))
                            .bg(accent()),
                    )
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_tab = tab;
                    cx.notify();
                }))
        };

        // Full-window backdrop; clicking outside the dialog closes it.
        div()
            .id("settings-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.settings_open = false;
                    this.bump_activity(cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("settings-modal")
                    .w(px(660.))
                    .max_w(px(660.))
                    .h(px(480.))
                    .max_h(px(480.))
                    .flex()
                    .flex_col()
                    .bg(panel())
                    .border_1()
                    .border_color(line_strong())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        // Header
                        div()
                            .h(px(40.))
                            .flex_none()
                            .px(px(14.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .bg(window_bg())
                            .border_b_1()
                            .border_color(line())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_size(px(13.))
                                            .child("设置"),
                                    )
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(11.))
                                            .text_color(text_faint())
                                            .child(if cfg!(target_os = "macos") {
                                                "⌘,"
                                            } else {
                                                "Ctrl+,"
                                            }),
                                    ),
                            )
                            .child(
                                icon_button("settings-close", Icon::Close, false)
                                    .tooltip(tip("关闭设置", Some("Esc")))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        this.bump_activity(cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        // Main split: left nav + right content
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .child(
                                div()
                                    .w(px(144.))
                                    .flex_none()
                                    .h_full()
                                    .py(px(8.))
                                    .bg(window_bg())
                                    .border_r_1()
                                    .border_color(line())
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(nav_item(SettingsTab::Appearance, "外观与布局"))
                                    .child(nav_item(SettingsTab::Playback, "播放"))
                                    .child(nav_item(SettingsTab::Tracks, "字幕与音频"))
                                    .child(nav_item(SettingsTab::View, "全屏与看图"))
                                    .child(nav_item(SettingsTab::Shortcuts, "快捷键与数据")),
                            )
                            .child(
                                div()
                                    .id("settings-scroll")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .h_full()
                                    .overflow_y_scroll()
                                    .p(px(18.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(14.))
                                    .child(body),
                            ),
                    ),
            )
    }

    fn settings_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.settings;
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(setting_row(
                "目录栏位置",
                "停靠和全屏弹窗时目录所在的一侧",
                segmented(
                    "set-dir-side",
                    &[("左侧", Side::Left), ("右侧", Side::Right)],
                    s.directory_side,
                    cx,
                    |this, val, cx| this.set_directory_side(val, cx),
                ),
            ))
            .child(setting_row(
                "目录栏宽度",
                "也可以直接拖拽目录边缘，双击边缘重置",
                stepper(
                    "set-dir-width",
                    format!("{:.0} px", s.directory_width),
                    (s.directory_width - DEFAULT_DIRECTORY_WIDTH).abs() > 1.,
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = (this.settings.directory_width - 20.)
                            .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
                        this.settings.save();
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = (this.settings.directory_width + 20.)
                            .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
                        this.settings.save();
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = DEFAULT_DIRECTORY_WIDTH;
                        this.settings.save();
                        cx.notify();
                    }),
                ),
            ))
            .child(setting_row(
                "启动时展开目录",
                "下次打开 Iris 时默认显示停靠目录",
                toggle(
                    "set-dir-open",
                    s.directory_open,
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_open = !this.settings.directory_open;
                        this.settings.save();
                        cx.notify();
                    }),
                ),
            ))
            .child(setting_row(
                "记住窗口尺寸",
                "退出时保存当前窗口宽高，下次启动自动恢复",
                toggle(
                    "set-win-size",
                    s.remember_window_size,
                    cx.listener(|this, _, _, cx| {
                        this.settings.remember_window_size = !this.settings.remember_window_size;
                        this.settings.save();
                        cx.notify();
                    }),
                ),
            ))
    }

    fn settings_playback(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    cx,
                    |this, val, cx| {
                        this.settings.on_eof = val;
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "方向键跳转步长",
                "按 ← / → 精确快退快进的秒数（Shift+←/→ 为 6 倍）",
                segmented(
                    "set-seek-step",
                    &[("3s", 3), ("5s", 5), ("10s", 10), ("15s", 15), ("30s", 30)],
                    s.seek_step.round() as i32,
                    cx,
                    |this, val, cx| {
                        this.settings.seek_step = val as f64;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "硬件解码",
                "遇到异常片源花屏时可切换为软解",
                segmented(
                    "set-hwdec",
                    &[("自动安全", Hwdec::AutoSafe), ("软解", Hwdec::Off)],
                    s.hwdec,
                    cx,
                    |this, val, cx| {
                        this.settings.hwdec = val;
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "记住播放进度",
                "再次打开同一视频时从上次位置续播",
                toggle(
                    "set-resume",
                    s.resume_playback,
                    cx.listener(|this, _, _, cx| {
                        this.settings.resume_playback = !this.settings.resume_playback;
                        this.settings.save();
                        cx.notify();
                    }),
                ),
            ))
            .child(setting_row(
                "启动时恢复倍速",
                "关闭后每次启动恢复为 1.00× 常速",
                toggle(
                    "set-speed",
                    s.remember_speed,
                    cx.listener(|this, _, _, cx| {
                        this.settings.remember_speed = !this.settings.remember_speed;
                        this.settings.save();
                        cx.notify();
                    }),
                ),
            ))
    }

    fn settings_tracks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.settings;
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
                    cx,
                    |this, val, cx| {
                        this.settings.sub_lang = val;
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "字幕字号缩放",
                "按比例放大或缩小视频字幕",
                stepper(
                    "set-sub-scale",
                    format!("{:.2}×", s.sub_scale),
                    (s.sub_scale - 1.0).abs() > 0.01,
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale =
                            ((this.settings.sub_scale - 0.1) * 100.).round() / 100.;
                        this.settings.sub_scale = this.settings.sub_scale.clamp(0.5, 2.5);
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale =
                            ((this.settings.sub_scale + 0.1) * 100.).round() / 100.;
                        this.settings.sub_scale = this.settings.sub_scale.clamp(0.5, 2.5);
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale = 1.0;
                        this.settings.save();
                        this.viewports[0].session.apply_settings(&this.settings);
                        cx.notify();
                    }),
                ),
            ))
            .child(setting_row(
                "视频画面滚轮行为",
                "指针在视频画面上方滚动滚轮时的响应",
                segmented(
                    "set-wheel",
                    &[("调节音量", VideoWheel::Volume), ("无操作", VideoWheel::None)],
                    s.video_wheel,
                    cx,
                    |this, val, cx| {
                        this.settings.video_wheel = val;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
    }

    fn settings_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    cx,
                    |this, val, cx| {
                        this.settings.fullscreen_hide_ms = val;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "照片默认视图",
                "打开新照片时的初始缩放模式",
                segmented(
                    "set-photo-default",
                    &[("适应窗口", PhotoDefault::Fit), ("原始 1:1", PhotoDefault::Actual)],
                    s.photo_default,
                    cx,
                    |this, val, cx| {
                        this.settings.photo_default = val;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
    }

    fn settings_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cmd = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let open_k = format!("{cmd}O");
        let refresh_k = format!("{cmd}R");
        let set_k = format!("{cmd},");
        let quit_k = format!("{cmd}Q");

        let groups: Vec<(&str, Vec<(String, &str)>)> = vec![
            (
                "通用",
                vec![
                    (open_k, "打开文件或文件夹"),
                    ("Tab".into(), "展开 / 收起目录（全屏为弹出）"),
                    ("F / 双击".into(), "切换全屏"),
                    ("Esc".into(), "关闭浮层 / 退出全屏"),
                    ("PgUp / PgDn".into(), "上一个 / 下一个媒体"),
                    ("S".into(), "收藏 / 取消收藏"),
                    (refresh_k, "刷新当前目录"),
                    (set_k, "打开 / 关闭设置"),
                    (quit_k, "退出"),
                ],
            ),
            (
                "视频",
                vec![
                    ("Space".into(), "播放 / 暂停（播完重播）"),
                    ("← / →".into(), "精确快退 / 快进"),
                    ("Shift+← / →".into(), "大步快退 / 快进"),
                    (", / .".into(), "逐帧后退 / 前进"),
                    ("↑ / ↓".into(), "调节音量"),
                    ("M".into(), "静音 / 取消静音"),
                    ("[ / ] / Backspace".into(), "减速 / 加速 / 恢复 1.0×"),
                    ("V / J".into(), "字幕显隐 / 切换字幕轨"),
                    ("A".into(), "切换音轨"),
                    ("L".into(), "单曲循环开关"),
                ],
            ),
            (
                "照片",
                vec![
                    ("← / →".into(), "上一张 / 下一张"),
                    ("滚轮 / = / -".into(), "缩放（以光标为中心）"),
                    ("拖拽".into(), "放大后平移"),
                    ("0 / Space".into(), "适应窗口 / 切换 1:1"),
                    ("R / Shift+R".into(), "顺 / 逆时针临时旋转 90°"),
                ],
            ),
        ];

        let cfg_path = settings_path();
        let log_file = logging::log_path();
        let cfg_text = cfg_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into());
        let reveal_label = if cfg!(target_os = "macos") {
            "在访达中显示"
        } else {
            "打开所在目录"
        };

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .children(groups.into_iter().map(|(title, items)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(text_dim())
                            .pb(px(4.))
                            .border_b_1()
                            .border_color(line())
                            .child(title),
                    )
                    .children(items.into_iter().map(|(k, desc)| {
                        div()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(12.))
                            .child(div().text_color(text_muted()).child(desc))
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(11.))
                                    .text_color(text())
                                    .px(px(6.))
                                    .py(px(1.))
                                    .bg(window_bg())
                                    .border_1()
                                    .border_color(line())
                                    .child(k),
                            )
                    }))
            }))
            .child(
                div()
                    .pt(px(6.))
                    .border_t_1()
                    .border_color(line())
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(text_dim())
                            .child("数据与文件"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(div().text_size(px(12.)).child("配置与记录"))
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(10.))
                                            .text_color(text_faint())
                                            .truncate()
                                            .child(cfg_text),
                                    ),
                            )
                            .when_some(cfg_path, |d, p| {
                                d.child(action_button("reveal-cfg", reveal_label).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        if p.exists() {
                                            cx.reveal_path(&p);
                                        } else if let Some(parent) = p.parent() {
                                            let _ = std::fs::create_dir_all(parent);
                                            cx.reveal_path(parent);
                                        }
                                    }),
                                ))
                            }),
                    )
                    .when_some(log_file, |d, lp| {
                        let text_lp = lp.display().to_string();
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(div().text_size(px(12.)).child("运行日志"))
                                        .child(
                                            div()
                                                .font_family(FONT_MONO)
                                                .text_size(px(10.))
                                                .text_color(text_faint())
                                                .truncate()
                                                .child(text_lp),
                                        ),
                                )
                                .child(action_button("reveal-log", reveal_label).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        if lp.exists() {
                                            cx.reveal_path(&lp);
                                        } else if let Some(parent) = lp.parent() {
                                            cx.reveal_path(parent);
                                        }
                                    }),
                                )),
                        )
                    })
                    .child(setting_row(
                        "播放进度历史",
                        "清除所有视频的续播时间点，收藏列表不受影响",
                        action_button("clear-progress", "清除进度").on_click(cx.listener(
                            |this, _, _, cx| {
                                let n = this.store.clear_progress();
                                this.show_toast(
                                    if n > 0 {
                                        format!("已清除 {n} 条播放进度")
                                    } else {
                                        "没有可清除的播放进度".into()
                                    },
                                    cx,
                                );
                            },
                        )),
                    )),
            )
    }
}

fn setting_row(title: &'static str, desc: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .pb(px(12.))
        .border_b_1()
        .border_color(line())
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(12.)).text_color(text()).child(title))
                .child(div().text_size(px(11.)).text_color(text_faint()).child(desc)),
        )
        .child(div().flex_none().child(control))
}

fn toggle(
    id: &'static str,
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(40.))
        .h(px(22.))
        .p(px(3.))
        .flex()
        .items_center()
        .when(on, |d| d.justify_end().bg(accent()).border_color(accent()))
        .when(!on, |d| {
            d.justify_start().bg(window_bg()).border_color(line_strong())
        })
        .border_1()
        .cursor_pointer()
        .on_click(on_click)
        .child(
            div()
                .size(px(14.))
                .bg(if on { text() } else { text_dim() }),
        )
}

fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(&'static str, T)],
    current: T,
    cx: &mut Context<IrisWindow>,
    on_select: impl Fn(&mut IrisWindow, T, &mut Context<IrisWindow>) + Copy + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .border_1()
        .border_color(line_strong())
        .bg(window_bg())
        .children(options.iter().enumerate().map(|(i, &(label, value))| {
            let active = value == current;
            div()
                .id((id, i))
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(11.))
                .when(i > 0, |d| d.border_l_1().border_color(line()))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_dim())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if value != current {
                        on_select(this, value, cx);
                    }
                }))
        }))
}

fn stepper(
    id: &'static str,
    display: String,
    can_reset: bool,
    on_dec: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_inc: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_reset: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .when(can_reset, |d| {
            d.child(
                div()
                    .id((id, 99usize))
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(text_dim())
                    .hover(|s| s.bg(hover()).text_color(text()))
                    .child("重置")
                    .on_click(on_reset),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .border_1()
                .border_color(line_strong())
                .bg(window_bg())
                .child(
                    div()
                        .id((id, 0usize))
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .border_r_1()
                        .border_color(line())
                        .hover(|s| s.bg(hover()))
                        .child(icon(Icon::Minus, 12., text_dim()))
                        .on_click(on_dec),
                )
                .child(
                    div()
                        .min_w(px(56.))
                        .px(px(8.))
                        .flex()
                        .justify_center()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(text())
                        .child(display),
                )
                .child(
                    div()
                        .id((id, 1usize))
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .border_l_1()
                        .border_color(line())
                        .hover(|s| s.bg(hover()))
                        .child(icon(Icon::Plus, 12., text_dim()))
                        .on_click(on_inc),
                ),
        )
}

fn action_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(24.))
        .px(px(10.))
        .flex()
        .items_center()
        .cursor_pointer()
        .bg(window_bg())
        .border_1()
        .border_color(line_strong())
        .text_size(px(11.))
        .text_color(text_muted())
        .hover(|s| s.bg(hover()).text_color(text()))
        .active(|s| s.bg(selected()))
        .child(label)
}
