//! Reusable form controls (`setting_row`, `toggle`, `checkbox`, `segmented`, `stepper`)
//! backed by [`ThemeTokens`] radii and colors.

use gpui::{
    App, ClickEvent, Context, Div, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};

use super::super::IrisWindow;
use super::icon;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

pub(in crate::ui) fn setting_row(
    title: impl Into<SharedString>,
    desc: impl Into<SharedString>,
    control: impl IntoElement,
    t: ThemeTokens,
) -> Div {
    let desc = desc.into();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x(px(16.))
        .gap_y(px(8.))
        .pb(px(12.))
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .flex_1()
                .min_w(px(160.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(t.text_md)
                        .text_color(t.text)
                        .child(title.into()),
                )
                .when(!desc.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(t.text_sm)
                            .text_color(t.text_faint)
                            .child(desc),
                    )
                }),
        )
        .child(div().flex_shrink_0().max_w_full().child(control))
}

/// Compact framed action/chip button (`active = true` highlights `t.accent` border).
pub(in crate::ui) fn chip_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(t.control_h_sm)
        .px(px(8.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.))
        .rounded(t.radius_sm)
        .cursor_pointer()
        .bg(if active { t.selected } else { t.panel })
        .border_1()
        .border_color(if active { t.accent } else { t.line_strong })
        .text_size(t.text_sm)
        .text_color(if active { t.text } else { t.text_muted })
        .hover(|s| s.bg(t.hover).text_color(t.text))
        .active(|s| s.bg(t.selected))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(label.into())
}

pub(in crate::ui) fn toggle(
    id: impl Into<ElementId>,
    on: bool,
    t: ThemeTokens,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(40.))
        .h(px(22.))
        .p(px(3.))
        .flex()
        .items_center()
        .rounded(t.radius_md)
        .border_1()
        .cursor_pointer()
        .when(on, |d| d.justify_end().bg(t.accent).border_color(t.accent))
        .when(!on, |d| {
            d.justify_start()
                .bg(t.window_bg)
                .border_color(t.line_strong)
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child(
            div()
                .size(px(14.))
                .rounded(t.radius_sm)
                .bg(if on { t.text } else { t.text_dim }),
        )
}

pub(in crate::ui) fn check_indicator(checked: bool, size: f32, t: ThemeTokens) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius_sm)
        .border_1()
        .border_color(if checked { t.accent } else { t.line_strong })
        .bg(if checked { t.accent } else { t.window_bg })
        .when(checked, |d| d.child(icon(Icon::Check, size - 4., t.text)))
}

pub(in crate::ui) fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    t: ThemeTokens,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child(check_indicator(checked, 15., t))
        .child(
            div()
                .text_size(t.text_md)
                .text_color(if checked { t.text } else { t.text_muted })
                .child(label.into()),
        )
}

pub(in crate::ui) fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(&'static str, T)],
    current: T,
    t: ThemeTokens,
    cx: &mut Context<IrisWindow>,
    on_select: impl Fn(&mut IrisWindow, T, &mut Context<IrisWindow>) + Copy + 'static,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .rounded(t.radius_md)
        .overflow_hidden()
        .border_1()
        .border_color(t.line_strong)
        .bg(t.window_bg)
        .children(options.iter().enumerate().map(|(i, &(label, value))| {
            let active = value == current;
            div()
                .id((id, i))
                .h(t.control_h_sm)
                .px(px(10.))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(t.text_sm)
                .when(i > 0, |d| d.border_l_1().border_color(t.line))
                .when(active, |d| d.bg(t.selected).text_color(t.text))
                .when(!active, |d| {
                    d.text_color(t.text_dim)
                        .hover(|s| s.bg(t.hover).text_color(t.text))
                })
                .child(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if value != current {
                        on_select(this, value, cx);
                    }
                }))
        }))
}

pub(in crate::ui) fn stepper_box(
    dec_id: impl Into<ElementId>,
    inc_id: impl Into<ElementId>,
    display: impl Into<SharedString>,
    bg: Rgba,
    t: ThemeTokens,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let step_btn = |btn_id: ElementId, glyph: Icon, right_border: bool| {
        div()
            .id(btn_id)
            .size(t.control_h_sm)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(right_border, |d| d.border_r_1().border_color(t.line))
            .when(!right_border, |d| d.border_l_1().border_color(t.line))
            .hover(|s| s.bg(t.hover))
            .child(icon(glyph, 12., t.text_dim))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .rounded(t.radius_md)
        .overflow_hidden()
        .border_1()
        .border_color(t.line_strong)
        .bg(bg)
        .child(step_btn(dec_id.into(), Icon::Minus, true).on_click(on_dec))
        .child(
            div()
                .min_w(px(52.))
                .px(px(7.))
                .flex()
                .justify_center()
                .font_family(FONT_MONO)
                .text_size(t.text_sm)
                .text_color(t.text)
                .child(display.into()),
        )
        .child(step_btn(inc_id.into(), Icon::Plus, false).on_click(on_inc))
}

pub(in crate::ui) fn stepper(
    id: &'static str,
    display: impl Into<SharedString>,
    can_reset: bool,
    t: ThemeTokens,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .when(can_reset, |d| {
            d.child(
                div()
                    .id((id, 99usize))
                    .h(t.control_h_sm)
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_size(t.text_sm)
                    .text_color(t.text_dim)
                    .hover(|s| s.bg(t.hover).text_color(t.text))
                    .child("重置")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(on_reset),
            )
        })
        .child(stepper_box(
            (id, 0usize),
            (id, 1usize),
            display,
            t.window_bg,
            t,
            on_dec,
            on_inc,
        ))
}
