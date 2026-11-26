//! Appearance & general settings tab: curated themes, `ResetAppearance` guardrail,
//! sidebar/dock/compact/pin layout toggles, dock button visibility, and data cleanup.

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, rgb,
};
use iris_core::logging;
use iris_core::settings::{
    DEFAULT_DIRECTORY_WIDTH, DEFAULT_UI_FONT_SCALE, MAX_DIRECTORY_WIDTH, MAX_UI_FONT_SCALE,
    MIN_DIRECTORY_WIDTH, MIN_UI_FONT_SCALE, Side, path as settings_path,
};

use super::super::IrisWindow;
use super::super::actions::ToggleAlwaysOnTop;
use super::super::kit::form::{checkbox, chip_button, segmented, setting_row, stepper, toggle};
use super::super::kit::{
    ButtonSize, ButtonVariant, badge_pill, bordered_button, button, hbetween, hstack, icon,
    section_label, vstack,
};
use crate::assets::Icon;
use crate::platform::REVEAL_IN_FILE_MANAGER_LABEL;
use crate::theme::{
    BUILTIN_THEMES, BuiltinThemeInfo, FONT_MONO, ThemeTokens, theme_preview_colors,
};

const DOCK_BUTTON_TOGGLES: &[(&str, &str)] = &[
    ("screenshot", "截图"),
    ("loop", "单曲循环"),
    ("aspect", "画面比例"),
    ("subtitles", "字幕"),
    ("audio", "音轨"),
    ("speed", "倍速"),
    ("info", "媒体统计"),
    ("favorite", "收藏"),
];

fn render_theme_card(
    card_ix: usize,
    th: &BuiltinThemeInfo,
    active: bool,
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    let (bg_hex, acc_hex) = theme_preview_colors(th.id);
    let (tid, tname) = (th.id, th.name);
    let swatch = div()
        .w(px(18.))
        .h(px(12.))
        .border_1()
        .border_color(t.line_strong)
        .bg(rgb(bg_hex))
        .flex()
        .items_center()
        .justify_end()
        .p(px(2.))
        .child(div().size(px(5.)).bg(rgb(acc_hex)));

    div()
        .id(("theme-card", card_ix))
        .flex_1()
        .min_w(px(0.))
        .p(px(9.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .rounded(t.radius_md)
        .cursor_pointer()
        .bg(if active { t.selected } else { t.window_bg })
        .border_1()
        .border_color(if active { t.accent } else { t.line })
        .hover(|st| st.bg(t.hover))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.settings.theme_id = tid.to_string();
            this.preset_rt.record_user_edit(&this.settings);
            this.sync_theme_tokens(cx);
            this.show_toast(format!("已切换主题 · {tname}"), cx);
            cx.notify();
        }))
        .child(
            hbetween(6.)
                .child(
                    hstack(6.)
                        .child(swatch)
                        .child(div().text_size(t.text_md).text_color(t.text).child(th.name)),
                )
                .when(active, |d| d.child(icon(Icon::Check, 12., t.accent))),
        )
        .child(
            div()
                .text_size(t.text_xs)
                .text_color(t.text_faint)
                .truncate()
                .child(th.description),
        )
        .into_any_element()
}

fn reveal_path_row(
    btn_id: &'static str,
    title: &'static str,
    display_path: String,
    target: Option<PathBuf>,
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    hbetween(10.)
        .child(
            vstack(2.)
                .flex_1()
                .min_w(px(0.))
                .child(div().text_size(t.text_md).child(title))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(t.text_xs)
                        .text_color(t.text_faint)
                        .truncate()
                        .child(display_path),
                ),
        )
        .when_some(target, |d, p| {
            d.child(
                bordered_button(btn_id, REVEAL_IN_FILE_MANAGER_LABEL, false, t).on_click(
                    cx.listener(move |_, _, _, cx| {
                        if p.exists() {
                            cx.reveal_path(&p);
                        } else if let Some(parent) = p.parent() {
                            let _ = std::fs::create_dir_all(parent);
                            cx.reveal_path(parent);
                        }
                    }),
                ),
            )
        })
        .into_any_element()
}

impl IrisWindow {
    pub(super) fn settings_appearance(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = &self.settings;
        let (ovr_theme, ovr_chrome) = self.plugins.active_ui_overrides();
        let has_plugin_override = ovr_theme.is_some() || ovr_chrome.is_some();
        let cfg_path = settings_path();
        let log_file = logging::log_path();
        let events_file = logging::events_log_path();
        let cfg_text = cfg_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into());

        let theme_rows: Vec<AnyElement> = BUILTIN_THEMES
            .chunks(2)
            .enumerate()
            .map(|(ri, pair)| {
                let cards: Vec<AnyElement> = pair
                    .iter()
                    .enumerate()
                    .map(|(ci, th)| {
                        let active = s.theme_id.eq_ignore_ascii_case(th.id)
                            || (th.id == "graphite"
                                && !BUILTIN_THEMES.iter().any(|x| {
                                    x.id != "graphite" && s.theme_id.eq_ignore_ascii_case(x.id)
                                }));
                        render_theme_card(ri * 2 + ci, th, active, t, cx)
                    })
                    .collect();
                div().flex().gap(px(6.)).children(cards).into_any_element()
            })
            .collect();

        let dock_checks: Vec<AnyElement> = DOCK_BUTTON_TOGGLES
            .iter()
            .enumerate()
            .map(|(bi, &(btn_id, label))| {
                let visible = !s
                    .hidden_dock_buttons
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(btn_id));
                checkbox(
                    ("dock-vis-btn", bi),
                    visible,
                    label,
                    t,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(pos) = this
                            .settings
                            .hidden_dock_buttons
                            .iter()
                            .position(|b| b.eq_ignore_ascii_case(btn_id))
                        {
                            this.settings.hidden_dock_buttons.remove(pos);
                        } else {
                            this.settings.hidden_dock_buttons.push(btn_id.to_string());
                        }
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                )
                .into_any_element()
            })
            .collect();

        let (dir_side, dir_w, dir_open) = (s.directory_side, s.directory_width, s.directory_open);
        let (floating_dock, compact_controls, show_meta, rem_win, top) = (
            s.floating_dock,
            s.compact_controls,
            s.show_titlebar_meta,
            s.remember_window_size,
            s.always_on_top,
        );

        let ui_scale = s.ui_font_scale;
        let confirm_recent = self.is_confirming("clear-recent");
        let confirm_progress = self.is_confirming("clear-progress");

        let data_sec = vstack(10.)
            .child(section_label("应用数据与历史清理", t))
            .child(reveal_path_row(
                "reveal-cfg",
                "配置与数据目录",
                cfg_text,
                cfg_path,
                t,
                cx,
            ))
            .when_some(log_file, |d, lp| {
                let disp = lp.display().to_string();
                d.child(reveal_path_row(
                    "reveal-log",
                    "运行日志（自动轮转）",
                    disp,
                    Some(lp),
                    t,
                    cx,
                ))
            })
            .when_some(events_file, |d, ep| {
                let disp = ep.display().to_string();
                d.child(reveal_path_row(
                    "reveal-events-log",
                    "结构化诊断事件（JSONL）",
                    disp,
                    Some(ep),
                    t,
                    cx,
                ))
            })
            .child(setting_row(
                "最近打开记录",
                "清除空窗口显示的最近打开文件与远程地址列表",
                button(
                    "clear-recent-btn",
                    if confirm_recent {
                        "再次点击确认"
                    } else {
                        "清除最近"
                    },
                    if confirm_recent {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Secondary
                    },
                    ButtonSize::Md,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.confirm_destructive("clear-recent", cx) {
                        this.store.clear_recent();
                        this.show_toast("已清除最近打开记录", cx);
                    }
                })),
                t,
            ))
            .child(setting_row(
                "播放进度历史",
                "清除所有本地与远程媒体的续播时间点，收藏列表不受影响",
                button(
                    "clear-progress",
                    if confirm_progress {
                        "再次点击确认"
                    } else {
                        "清除进度"
                    },
                    if confirm_progress {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Secondary
                    },
                    ButtonSize::Md,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.confirm_destructive("clear-progress", cx) {
                        let n = this.store.clear_progress();
                        let msg = if n > 0 {
                            format!("已清除 {n} 条播放进度")
                        } else {
                            "没有可清除的播放进度".into()
                        };
                        this.show_toast(msg, cx);
                    }
                })),
                t,
            ));

        vstack(14.)
            .child(
                vstack(8.)
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        hbetween(8.)
                            .child(
                                vstack(2.)
                                    .child(
                                        div()
                                            .text_size(t.text_md)
                                            .text_color(t.text)
                                            .child("色彩与面板主题"),
                                    )
                                    .child(
                                        div()
                                            .text_size(t.text_sm)
                                            .text_color(t.text_faint)
                                            .child("内置 4 套工作台色彩方案，实时无重启切换"),
                                    ),
                            )
                            .child(
                                hstack(6.)
                                    .when(has_plugin_override, |d| {
                                        d.child(badge_pill("插件外观覆盖中", true, t))
                                    })
                                    .child(
                                        chip_button(
                                            "reset-appearance-btn",
                                            "恢复默认外观",
                                            false,
                                            t,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, w, cx| {
                                                this.reset_appearance(w, cx);
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(vstack(6.).children(theme_rows)),
            )
            .child(setting_row(
                "界面字体缩放",
                "全局缩放界面字号与控件行高（85%–135%），实时生效且不随预设覆盖",
                stepper(
                    "set-ui-font-scale",
                    format!("{:.0}%", ui_scale * 100.),
                    (ui_scale - DEFAULT_UI_FONT_SCALE).abs() > 0.01,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.ui_font_scale =
                            (((this.settings.ui_font_scale - 0.05) * 100.).round() / 100.)
                                .clamp(MIN_UI_FONT_SCALE, MAX_UI_FONT_SCALE);
                        this.preset_rt.record_user_edit(&this.settings);
                        this.sync_theme_tokens(cx);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.ui_font_scale =
                            (((this.settings.ui_font_scale + 0.05) * 100.).round() / 100.)
                                .clamp(MIN_UI_FONT_SCALE, MAX_UI_FONT_SCALE);
                        this.preset_rt.record_user_edit(&this.settings);
                        this.sync_theme_tokens(cx);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.ui_font_scale = DEFAULT_UI_FONT_SCALE;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.sync_theme_tokens(cx);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "侧栏停靠位置",
                "全屏时将鼠标移到这一侧的屏幕边缘可展开侧栏，移回画面可收起",
                segmented(
                    "set-dir-side",
                    &[("左侧", Side::Left), ("右侧", Side::Right)],
                    dir_side,
                    t,
                    cx,
                    |this, val, cx| this.set_directory_side(val, cx),
                ),
                t,
            ))
            .child(setting_row(
                "侧栏面板宽度",
                "也可直接拖拽侧栏边缘调节",
                stepper(
                    "set-dir-width",
                    format!("{dir_w:.0} px"),
                    (dir_w - DEFAULT_DIRECTORY_WIDTH).abs() > 1.,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = (this.settings.directory_width - 20.)
                            .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = (this.settings.directory_width + 20.)
                            .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_width = DEFAULT_DIRECTORY_WIDTH;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "显示停靠侧栏",
                "在窗口内停靠显示目录、数据源、队列与插件面板",
                toggle(
                    "set-dir-open",
                    dir_open,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.directory_open = !this.settings.directory_open;
                        this.preset_rt.record_user_edit(&this.settings);
                        this.sync_view_timer(cx);
                        if this.settings.directory_open {
                            this.scroll_directory_to_current();
                        }
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "窗口悬浮控制栏",
                "窗口模式下也将底部播放控制栏悬浮在画面上方",
                toggle(
                    "set-floating-dock",
                    floating_dock,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.floating_dock = !this.settings.floating_dock;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "紧凑控制栏模式",
                "收紧底栏按钮间距与尺寸，为迷你小窗留出更多画面空间",
                toggle(
                    "set-compact-controls",
                    compact_controls,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.compact_controls = !this.settings.compact_controls;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "标题栏显示编码元信息",
                "在顶部标题栏右侧展示分辨率、硬解状态或色彩空间",
                toggle(
                    "set-titlebar-meta",
                    show_meta,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.show_titlebar_meta = !this.settings.show_titlebar_meta;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "记住窗口尺寸",
                "退出时保存当前窗口宽高，下次启动自动恢复",
                toggle(
                    "set-win-size",
                    rem_win,
                    t,
                    cx.listener(|this, _, _, cx| {
                        this.settings.remember_window_size = !this.settings.remember_window_size;
                        this.preset_rt.record_user_edit(&this.settings);
                        cx.notify();
                    }),
                ),
                t,
            ))
            .child(setting_row(
                "窗口置顶",
                "使播放器窗口悬浮在其他应用上方",
                toggle(
                    "set-top",
                    top,
                    t,
                    cx.listener(|this, _, w, cx| {
                        this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx)
                    }),
                ),
                t,
            ))
            .child(
                vstack(8.)
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        vstack(2.)
                            .child(
                                div()
                                    .text_size(t.text_md)
                                    .text_color(t.text)
                                    .child("底栏常驻按钮显隐"),
                            )
                            .child(
                                div()
                                    .text_size(t.text_sm)
                                    .text_color(t.text_faint)
                                    .child("勾选要在底部控制栏常驻显示的功能按钮"),
                            ),
                    )
                    .child(div().flex().flex_wrap().gap(px(12.)).children(dock_checks)),
            )
            .child(data_sec)
    }
}
