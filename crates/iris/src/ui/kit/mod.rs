//! Shared GPUI component kit: icons, buttons, badges, separators, tooltips,
//! text inputs, form controls, virtualized list rows, and modal shells.

pub(super) mod form;
pub(super) mod list_row;
pub(super) mod modal;
pub(super) mod text_buffer;
pub(super) mod text_field;

use gpui::{
    AnyView, App, AppContext as _, Context, Div, ElementId, FontWeight, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, Rgba, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Svg, Window, div, prelude::FluentBuilder, px, svg,
};

use crate::assets::Icon;
use crate::theme::{BUTTON, FONT_MONO, FONT_UI, ThemeTokens};

/// Group name registered by every ghost button so its icon brightens on hover.
const GHOST: &str = "ghost";

pub(super) fn icon(glyph: Icon, size: f32, color: Rgba) -> Svg {
    svg()
        .path(glyph.path())
        .size(px(size))
        .flex_none()
        .text_color(color)
}

/// Borderless square/pill button at rest; filled on hover or when `on` is active.
pub(super) fn ghost(id: impl Into<ElementId>, on: bool, t: ThemeTokens) -> Stateful<Div> {
    div()
        .id(id)
        .group(GHOST)
        .h(px(BUTTON))
        .min_w(px(BUTTON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius_sm)
        .cursor_pointer()
        .when(on, |d| d.bg(t.selected).text_color(t.text))
        .when(!on, |d| {
            d.text_color(t.text_dim)
                .hover(|s| s.bg(t.hover).text_color(t.text))
        })
        .active(|s| s.bg(t.selected))
}

/// Icon inside a [`ghost`] button that brightens to `t.text` on hover.
pub(super) fn icon_on_ghost(glyph: Icon, color: Rgba, t: ThemeTokens) -> Svg {
    icon(glyph, 16., color).group_hover(GHOST, move |s| s.text_color(t.text))
}

/// Square ghost button holding a single 16px icon.
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    on: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    ghost(id, on, t).child(icon_on_ghost(
        glyph,
        if on { t.text } else { t.text_dim },
        t,
    ))
}

/// Ghost button labelled in monospace (`1:1`, `1.00×`).
pub(super) fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    t: ThemeTokens,
) -> Stateful<Div> {
    ghost(id, false, t)
        .px(px(6.))
        .font_family(FONT_MONO)
        .text_size(px(11.))
        .child(label.into())
}

/// Framed action button (`primary = true` fills with `t.accent`).
pub(super) fn bordered_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(12.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(t.radius_md)
        .border_1()
        .cursor_pointer()
        .text_size(px(12.))
        .when(primary, |d| {
            d.bg(t.accent)
                .border_color(t.accent)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.window_bg)
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.bg(t.panel)
                .border_color(t.line_strong)
                .text_color(t.text_muted)
                .hover(|s| s.bg(t.hover).text_color(t.text))
                .active(|s| s.bg(t.selected))
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(label.into())
}

/// Compact monospace badge pill (`LIVE`, count, or preset status).
pub(super) fn badge_pill(label: impl Into<SharedString>, accent: bool, t: ThemeTokens) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(t.radius_sm)
        .border_1()
        .border_color(if accent { t.accent } else { t.line })
        .bg(if accent { t.selected } else { t.window_bg })
        .font_family(FONT_MONO)
        .text_size(px(10.))
        .text_color(if accent { t.accent } else { t.text_dim })
        .child(label.into())
}

/// 1px vertical divider between button groups.
pub(super) fn separator(t: ThemeTokens) -> Div {
    div().flex_none().w(px(1.)).h(px(16.)).mx(px(6.)).bg(t.line)
}

/// Horizontal centered flex row with `gap` in logical pixels.
pub(super) fn hstack(gap: f32) -> Div {
    div().flex().items_center().gap(px(gap))
}

/// Vertical flex column with `gap` in logical pixels.
pub(super) fn vstack(gap: f32) -> Div {
    div().flex().flex_col().gap(px(gap))
}

/// Space-between horizontal flex row with `gap` in logical pixels.
pub(super) fn hbetween(gap: f32) -> Div {
    div().flex().items_center().justify_between().gap(px(gap))
}

struct Tip {
    label: SharedString,
    key: Option<SharedString>,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        div().pt(px(8.)).px(px(8.)).pb(px(28.)).child(
            div()
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(t.radius_sm)
                .bg(t.panel)
                .border_1()
                .border_color(t.line_strong)
                .font_family(FONT_UI)
                .text_size(px(12.))
                .text_color(t.text)
                .whitespace_nowrap()
                .child(self.label.clone())
                .when_some(self.key.clone(), |d, key| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(t.text_dim)
                            .child(key),
                    )
                }),
        )
    }
}

pub(super) fn tip(
    label: impl Into<SharedString>,
    key_hint: Option<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |_, cx| {
        cx.new(|_| Tip {
            label: label.clone(),
            key: key_hint.clone(),
        })
        .into()
    }
}
