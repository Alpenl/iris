//! Windowed and frameless-overlay title bar: platform window buttons, drag area,
//! media info, workspace preset pill with `· 已微调` baseline badge, and pin toggle.

use gpui::{
    App, Entity, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px,
};

use super::WindowShell;
use super::actions::ToggleAlwaysOnTop;
use super::kit::{icon, tip};
use super::state::PopoverKind;
use super::views::model_listener;
use crate::assets::Icon;
use crate::platform;
use crate::theme::{FONT_MONO, ThemeTokens};

pub(super) fn render_titlebar(
    model: &Entity<WindowShell>,
    overlay_mode: bool,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let session = m.session();
    let file = session
        .current()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| session.current_title().map(str::to_string));
    let remote_badge = !m.catalog().active_source().is_local();
    let folder = m
        .catalog()
        .titlebar_folder_label(session.current_uri().is_some());
    let pinned = m.settings().always_on_top;
    let modal_open = m.has_active_modal();

    let preset_open = m.overlays.popover == Some(PopoverKind::Presets);
    let non_default =
        !m.settings().active_preset.is_empty() && m.settings().active_preset != "default";
    let preset_modified = m.is_preset_modified();
    let preset_hi = preset_open || non_default;
    let base_name =
        super::strings::presets::preset_name_by_id(m.presets(), &m.settings().active_preset);
    let preset_label = if preset_modified {
        format!("{base_name} · 已微调")
    } else {
        base_name.to_string()
    };
    let preset_slot = m.chrome.probes.anchor_presets.clone();
    let cycle_preset_hint = m.key_hint("cycle_preset");
    let pin_hint = m.key_hint("toggle_always_on_top");

    let win_btn = |id: &'static str, glyph: Icon, label: &'static str, danger_hover: bool| {
        div()
            .id(id)
            .group(id)
            .size(px(22.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.radius_md)
            .border_1()
            .border_color(t.line)
            .cursor_pointer()
            .tooltip(tip(label, None))
            .when(danger_hover, |d| {
                d.hover(|s| s.bg(t.danger).border_color(t.danger))
            })
            .when(!danger_hover, |d| {
                d.hover(|s| s.bg(t.hover).border_color(t.line_strong))
            })
            .child(icon(glyph, 11., t.text_dim).group_hover(id, move |s| s.text_color(t.text)))
    };

    let preset_pill = div()
        .relative()
        .flex_shrink()
        .min_w(px(0.))
        .max_w(px(164.))
        .child(
            div()
                .id((
                    "win-preset",
                    usize::from(preset_open) * 2 + usize::from(non_default),
                ))
                .group("win-preset")
                .h(px((f32::from(t.control_h_sm) - 2.).max(22.)))
                .px(px(7.))
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(5.))
                .rounded(t.radius_md)
                .border_1()
                .border_color(if preset_open {
                    t.accent
                } else if non_default {
                    t.line_strong
                } else {
                    t.line
                })
                .when(preset_open, |d| d.bg(t.selected))
                .when(!preset_open, |d| {
                    d.bg(t.panel).when(!modal_open, |d| {
                        d.hover(|s| s.bg(t.hover).border_color(t.line_strong))
                    })
                })
                .tooltip(tip(
                    "工作区预设 · 点击切换或保存当前布局",
                    cycle_preset_hint,
                ))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .when(!modal_open, |d| {
                    d.cursor_pointer()
                        .on_click(model_listener(model, |this, _, _, cx| {
                            this.toggle_popover(PopoverKind::Presets, cx);
                        }))
                })
                .child(
                    icon(
                        Icon::Layers,
                        12.,
                        if preset_hi { t.accent } else { t.text_dim },
                    )
                    .group_hover("win-preset", move |s| {
                        s.text_color(if preset_hi { t.accent } else { t.text })
                    }),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(t.text_sm)
                        .text_color(if preset_hi { t.text } else { t.text_muted })
                        .group_hover("win-preset", move |s| s.text_color(t.text))
                        .child(preset_label),
                ),
        )
        .child(
            canvas(move |b, _, _| preset_slot.set(Some(b)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        );

    let pin_btn = div()
        .id(("win-pin", usize::from(pinned)))
        .group("win-pin")
        .size(px(22.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius_md)
        .border_1()
        .border_color(if pinned { t.accent } else { t.line })
        .when(pinned, |d| d.bg(t.selected))
        .when(!pinned && !modal_open, |d| {
            d.hover(|s| s.bg(t.hover).border_color(t.line_strong))
        })
        .tooltip(tip(
            if pinned {
                "取消窗口置顶"
            } else {
                "窗口置顶"
            },
            pin_hint,
        ))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .when(!modal_open, |d| {
            d.cursor_pointer()
                .on_click(model_listener(model, |this, _, w, cx| {
                    this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx);
                }))
        })
        .child(
            icon(Icon::Pin, 12., if pinned { t.accent } else { t.text_dim })
                .group_hover("win-pin", move |s| {
                    s.text_color(if pinned { t.accent } else { t.text })
                }),
        );

    let mac_buttons = || {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(10.))
            .child(
                win_btn("win-close", Icon::Close, "关闭", true).on_click(model_listener(
                    model,
                    |this, _, window, _| {
                        this.shutdown();
                        window.remove_window();
                    },
                )),
            )
            .child(
                win_btn("win-min", Icon::Minimize, "最小化", false)
                    .on_click(|_, window, _| window.minimize_window()),
            )
            .child(
                win_btn("win-full", Icon::Maximize, "全屏", false)
                    .on_click(|_, window, _| window.toggle_fullscreen()),
            )
    };

    let win_buttons = || {
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .pr(px(10.))
            .child(
                win_btn("win-min", Icon::Minimize, "最小化", false)
                    .on_click(|_, window, _| window.minimize_window()),
            )
            .child(
                win_btn("win-max", Icon::Maximize, "最大化 / 还原", false)
                    .on_click(|_, window, _| platform::toggle_maximize(window)),
            )
            .child(
                win_btn("win-close", Icon::Close, "关闭", true).on_click(model_listener(
                    model,
                    |this, _, window, _| {
                        this.shutdown();
                        window.remove_window();
                    },
                )),
            )
    };

    let meta_info = m
        .effective_show_titlebar_meta()
        .then(|| m.playback().media_info())
        .flatten();

    let drag_area = div()
        .id("drag")
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .flex()
        .items_center()
        .gap(px(10.))
        .pl(px(if platform::IS_MACOS { 12. } else { 14. }))
        .pr(px(10.))
        .on_mouse_down(
            MouseButton::Left,
            model_listener(model, |this, ev: &MouseDownEvent, window, _| {
                if ev.click_count >= 2 {
                    this.active_mut().pending_win_drag_origin = None;
                    platform::titlebar_double_click(window);
                } else if platform::IS_MACOS {
                    platform::begin_window_drag(window);
                } else {
                    this.active_mut().pending_win_drag_origin = Some(ev.position);
                }
            }),
        )
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::BOLD)
                .text_size(t.text_lg)
                .child("IRIS"),
        )
        .child(div().flex_none().w(px(1.)).h(px(14.)).bg(t.line))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(6.))
                .font_family(FONT_MONO)
                .text_size(t.text_sm)
                .child(
                    div()
                        .flex_shrink()
                        .min_w(px(0.))
                        .max_w(px(140.))
                        .truncate()
                        .text_color(if remote_badge { t.accent } else { t.text_dim })
                        .child(folder),
                )
                .when_some(file, |d, file| {
                    d.child(div().flex_none().text_color(t.text_faint).child("/"))
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(t.text)
                                .child(file),
                        )
                }),
        )
        .when_some(meta_info, |d, info| {
            d.child(
                div()
                    .flex_shrink()
                    .min_w(px(0.))
                    .max_w(px(180.))
                    .truncate()
                    .font_family(FONT_MONO)
                    .text_size(t.text_sm)
                    .text_color(t.text_dim)
                    .child(info),
            )
        })
        .child(preset_pill)
        .child(pin_btn);

    let bar = div()
        .h(t.titlebar_h)
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .bg(if overlay_mode { t.overlay } else { t.window_bg })
        .border_b_1()
        .border_color(t.line)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    if platform::IS_MACOS {
        bar.child(mac_buttons()).child(drag_area)
    } else {
        bar.child(drag_area).child(win_buttons())
    }
}
