//! In-window centered modal settings panel (Direction C: 1px borders, sharp
//! corners, hand-written toggle / segmented / stepper controls, responsive on
//! mini-windows).
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use super::widgets::{icon, icon_button, tip};
use super::*;
use crate::assets::Icon;
use crate::logging;
use crate::plugin::{PluginSource, PluginStatus, plugins_dir};
use crate::settings::{
    DEFAULT_DIRECTORY_WIDTH, Hwdec, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault,
    Side, SubLang, VideoWheel, path as settings_path,
};

impl IrisWindow {
    pub(super) fn render_settings_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let vp = window.viewport_size();
        let modal_w = (f32::from(vp.width) - 24.).clamp(420., 660.);
        let modal_h = (f32::from(vp.height) - 24.).clamp(260., 496.);

        let body: AnyElement = match self.overlays.settings_tab {
            SettingsTab::Appearance => self.settings_appearance(cx).into_any_element(),
            SettingsTab::Playback => self.settings_playback(cx).into_any_element(),
            SettingsTab::Tracks => self.settings_tracks(cx).into_any_element(),
            SettingsTab::View => self.settings_view(cx).into_any_element(),
            SettingsTab::Plugins => self.settings_plugins(cx).into_any_element(),
            SettingsTab::Shortcuts => self.settings_shortcuts(cx).into_any_element(),
        };

        let nav_item = |tab: SettingsTab, label: &'static str| {
            let active = self.overlays.settings_tab == tab;
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
                    this.overlays.settings_tab = tab;
                    cx.notify();
                }))
        };

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
                    this.overlays.settings_open = false;
                    this.bump_activity(cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("settings-modal")
                    .w(px(modal_w))
                    .h(px(modal_h))
                    .flex()
                    .flex_col()
                    .bg(panel())
                    .border_1()
                    .border_color(line_strong())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
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
                                        this.overlays.settings_open = false;
                                        this.bump_activity(cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
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
                                    .child(nav_item(SettingsTab::Plugins, "插件"))
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
                "显示停靠目录",
                "在窗口内停靠显示文件目录与插件侧栏，状态自动记住",
                toggle(
                    "set-dir-open",
                    s.directory_open,
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_open = !this.settings.directory_open;
                        this.settings.save();
                        if this.settings.directory_open {
                            this.scroll_directory_to_current();
                        }
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
            .child(setting_row(
                "窗口置顶",
                "使播放器窗口悬浮在其他应用上方（快捷键 T）",
                toggle(
                    "set-top",
                    s.always_on_top,
                    cx.listener(|this, _, w, cx| {
                        this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx);
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
                        this.active().session.set_on_eof_loop(val);
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
                "单击画面暂停 / 播放",
                "左键单击视频画面切换播放状态，双击仍为全屏",
                toggle(
                    "set-click-pause",
                    s.single_click_pause,
                    cx.listener(|this, _, _, cx| {
                        this.settings.single_click_pause = !this.settings.single_click_pause;
                        this.settings.save();
                        cx.notify();
                    }),
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
                        this.active().session.set_hwdec(val);
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
            .child(setting_row(
                "截图包含字幕",
                "关闭后截图仅保存原始视频帧画面",
                toggle(
                    "set-shot-subs",
                    s.screenshot_subs,
                    cx.listener(|this, _, _, cx| {
                        this.settings.screenshot_subs = !this.settings.screenshot_subs;
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
                "多字幕轨视频自动选择的语言顺序（自动加载同目录模糊匹配外挂字幕）",
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
                        this.active().session.set_sub_lang(val);
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
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale =
                            ((this.settings.sub_scale + 0.1) * 100.).round() / 100.;
                        this.settings.sub_scale = this.settings.sub_scale.clamp(0.5, 2.5);
                        this.settings.save();
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.sub_scale = 1.0;
                        this.settings.save();
                        this.active().session.set_sub_scale(this.settings.sub_scale);
                        cx.notify();
                    }),
                ),
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
                    &[
                        ("适应窗口", PhotoDefault::Fit),
                        ("原始 1:1", PhotoDefault::Actual),
                    ],
                    s.photo_default,
                    cx,
                    |this, val, cx| {
                        this.settings.photo_default = val;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "看图双击行为",
                "双击照片时切换原始大小 1:1 或切换全屏",
                segmented(
                    "set-photo-dbl",
                    &[("切换 1:1", true), ("切换全屏", false)],
                    s.photo_double_click_actual,
                    cx,
                    |this, val, cx| {
                        this.settings.photo_double_click_actual = val;
                        this.settings.save();
                        cx.notify();
                    },
                ),
            ))
            .child(setting_row(
                "照片底部缩略图条",
                "浏览多张照片时在画面下方显示内存缩略图快速跳转（快捷键 G）",
                toggle(
                    "set-filmstrip",
                    s.photo_filmstrip,
                    cx.listener(|this, _, w, cx| {
                        this.toggle_filmstrip(&ToggleFilmstrip, w, cx);
                    }),
                ),
            ))
    }

    fn settings_plugins(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.plugins.list();
        let dir = plugins_dir();
        let dir_text = dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into());
        let reveal_label = if cfg!(target_os = "macos") {
            "打开插件目录"
        } else {
            "打开插件文件夹"
        };

        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .pb(px(10.))
                    .border_b_1()
                    .border_color(line())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(text())
                                    .child("控制面与矢量叠层插件"),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(10.))
                                    .text_color(text_faint())
                                    .truncate()
                                    .child(dir_text),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(action_button("reload-plugins", "重新扫描").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.reload_plugins(cx);
                                }),
                            ))
                            .when_some(dir, |d, p| {
                                d.child(action_button("open-plugins-dir", reveal_label).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        let _ = std::fs::create_dir_all(&p);
                                        cx.reveal_path(&p);
                                    }),
                                ))
                            }),
                    ),
            )
            .children(list.into_iter().enumerate().map(|(i, info)| {
                let pid = info.manifest.id.clone();
                let enabled = info.enabled;
                let source_tag = match info.source {
                    PluginSource::Builtin => "内置",
                    PluginSource::External(_) => "外部进程",
                };
                let (status_text, status_accent) = match &info.status {
                    PluginStatus::Running => ("运行中".to_string(), false),
                    PluginStatus::Disabled => ("已禁用".to_string(), false),
                    PluginStatus::Error(err) => (format!("错误: {err}"), true),
                };
                div()
                    .p(px(10.))
                    .bg(window_bg())
                    .border_1()
                    .border_color(line())
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(text())
                                            .child(info.manifest.name),
                                    )
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(10.))
                                            .text_color(text_faint())
                                            .child(format!("v{}", info.manifest.version)),
                                    )
                                    .child(
                                        div()
                                            .px(px(5.))
                                            .py(px(1.))
                                            .bg(panel())
                                            .border_1()
                                            .border_color(line())
                                            .text_size(px(10.))
                                            .text_color(text_dim())
                                            .child(source_tag),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(if status_accent {
                                                accent()
                                            } else {
                                                text_faint()
                                            })
                                            .child(status_text),
                                    ),
                            )
                            .child(toggle_indexed(
                                ("plugin-toggle", i),
                                enabled,
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_plugin_enabled(&pid, !enabled, cx);
                                }),
                            )),
                    )
                    .when(!info.manifest.description.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(text_dim())
                                .child(info.manifest.description),
                        )
                    })
                    .child(div().flex().flex_wrap().gap(px(4.)).children(
                        info.manifest.permissions.into_iter().map(|perm| {
                            div()
                                .px(px(5.))
                                .py(px(1.))
                                .bg(panel())
                                .border_1()
                                .border_color(line())
                                .text_size(px(10.))
                                .text_color(text_faint())
                                .child(perm.label())
                        }),
                    ))
            }))
    }

    fn settings_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cmd = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let open_k = format!("{cmd}O");
        let pal_k = format!("{cmd}P");
        let search_k = format!("{cmd}F");
        let refresh_k = format!("{cmd}R");
        let shot_k = format!("{cmd}S");
        let set_k = format!("{cmd},");
        let quit_k = format!("{cmd}Q");

        let groups: Vec<(&str, Vec<(String, &str)>)> = vec![
            (
                "通用",
                vec![
                    (pal_k, "命令面板（内核与插件命令）"),
                    (open_k, "打开文件或文件夹"),
                    (search_k, "搜索当前目录"),
                    ("Tab".into(), "展开 / 收起目录（全屏为弹出）"),
                    ("F / 双击".into(), "切换全屏"),
                    ("T".into(), "窗口置顶开关"),
                    ("I".into(), "媒体详情与统计面板"),
                    ("Esc".into(), "关闭浮层 / 退出全屏"),
                    ("PgUp / PgDn".into(), "上一个 / 下一个媒体"),
                    ("S".into(), "收藏 / 取消收藏"),
                    (refresh_k, "刷新当前目录"),
                    (set_k, "打开 / 关闭设置"),
                    (quit_k, "退出"),
                ],
            ),
            (
                "视频 / 音频",
                vec![
                    ("Space / 单击".into(), "播放 / 暂停（播完重播）"),
                    ("← / →".into(), "精确快退 / 快进"),
                    ("Shift+← / →".into(), "大步快退 / 快进"),
                    (", / .".into(), "逐帧后退 / 前进"),
                    ("↑ / ↓".into(), "调节音量"),
                    ("M".into(), "静音 / 取消静音"),
                    ("[ / ] / Backspace".into(), "减速 / 加速 / 恢复 1.0×"),
                    ("V / J".into(), "字幕显隐 / 切换字幕轨"),
                    ("Z / X / Shift+Z".into(), "字幕偏移 −0.1s / +0.1s / 归零"),
                    ("A".into(), "切换音轨"),
                    ("L".into(), "单曲循环开关"),
                    ("C".into(), "切换画面比例 / 铺满裁剪"),
                    (
                        "R / Shift+R / H / Shift+H".into(),
                        "顺逆时针旋转 90° / 水平垂直翻转",
                    ),
                    (shot_k, "保存当前帧截图"),
                    ("Shift+[ / ]".into(), "上一章节 / 下一章节"),
                ],
            ),
            (
                "照片",
                vec![
                    ("← / →".into(), "上一张 / 下一张"),
                    ("滚轮 / 捏合 / = / -".into(), "以光标为中心缩放"),
                    ("拖拽".into(), "放大后平移"),
                    ("0 / Space / 双击".into(), "适应窗口 / 切换 1:1"),
                    (
                        "R / Shift+R / H / Shift+H".into(),
                        "顺逆时针旋转 90° / 水平垂直翻转",
                    ),
                    ("G".into(), "显示 / 隐藏底部缩略图条"),
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
                        "最近打开记录",
                        "清除空窗口显示的最近打开文件与文件夹列表",
                        action_button("clear-recent-btn", "清除最近").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.store.clear_recent();
                                this.show_toast("已清除最近打开记录", cx);
                            },
                        )),
                    ))
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

fn setting_row(
    title: &'static str,
    desc: &'static str,
    control: impl IntoElement,
) -> impl IntoElement {
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
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(text_faint())
                        .child(desc),
                ),
        )
        .child(div().flex_none().child(control))
}

fn toggle(
    id: &'static str,
    on: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    toggle_indexed(id, on, on_click)
}

fn toggle_indexed(
    id: impl Into<gpui::ElementId>,
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
            d.justify_start()
                .bg(window_bg())
                .border_color(line_strong())
        })
        .border_1()
        .cursor_pointer()
        .on_click(on_click)
        .child(div().size(px(14.)).bg(if on { text() } else { text_dim() }))
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
