//! Shared GPUI component kit: icons, buttons, badges, cards, headers, tab strips,
//! separators, tooltips, text inputs, form controls, virtualized list rows, and modal shells.

pub(super) mod form;
pub(super) mod list_row;
pub(super) mod modal;
pub(super) mod text_buffer;
pub(super) mod text_field;

use gpui::{
    AnyElement, AnyView, App, AppContext as _, Context, Div, ElementId, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Rgba, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Svg, Window, div, prelude::FluentBuilder, px,
    svg,
};

use crate::assets::Icon;
use crate::theme::{FONT_MONO, FONT_UI, SPACE_MD, SPACE_SM, SPACE_XS, ThemeTokens};

/// Group name registered by every ghost button so its icon brightens on hover.
const GHOST: &str = "ghost";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum ButtonSize {
    Xs,
    Sm,
    Md,
}

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
        .h(t.control_h_md)
        .min_w(t.control_h_md)
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

/// Non-interactive muted icon button for boundary/disabled toolbar states.
pub(super) fn icon_button_disabled(
    id: impl Into<ElementId>,
    glyph: Icon,
    on: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(t.control_h_md)
        .min_w(t.control_h_md)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius_sm)
        .opacity(0.45)
        .cursor_default()
        .when(on, |d| d.bg(t.selected))
        .child(icon(glyph, 16., t.text_dim))
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
        .text_size(t.text_sm)
        .child(label.into())
}

/// Unified action button with 4 semantic variants and 3 scale-aware sizes.
pub(in crate::ui) fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    variant: ButtonVariant,
    size: ButtonSize,
    t: ThemeTokens,
) -> Stateful<Div> {
    let (h, pad_x, text_sz) = match size {
        ButtonSize::Xs => (
            px((f32::from(t.control_h_sm) - 2.).max(20.)),
            px(7.),
            t.text_xs,
        ),
        ButtonSize::Sm => (t.control_h_sm, px(SPACE_SM), t.text_sm),
        ButtonSize::Md => (t.control_h_md, px(SPACE_MD), t.text_md),
    };
    let radius = match size {
        ButtonSize::Xs | ButtonSize::Sm => t.radius_sm,
        ButtonSize::Md => t.radius_md,
    };
    let base = div()
        .id(id)
        .h(h)
        .px(pad_x)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(radius)
        .cursor_pointer()
        .text_size(text_sz)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    let styled = match variant {
        ButtonVariant::Primary => base
            .border_1()
            .bg(t.accent)
            .border_color(t.accent)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.window_bg)
            .hover(|s| s.opacity(0.9)),
        ButtonVariant::Secondary => base
            .border_1()
            .bg(t.panel)
            .border_color(t.line_strong)
            .text_color(t.text_muted)
            .hover(|s| s.bg(t.hover).text_color(t.text))
            .active(|s| s.bg(t.selected)),
        ButtonVariant::Ghost => base
            .text_color(t.text_muted)
            .hover(|s| s.bg(t.hover).text_color(t.text))
            .active(|s| s.bg(t.selected)),
        ButtonVariant::Danger => base
            .border_1()
            .bg(t.window_bg)
            .border_color(t.danger)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.danger)
            .hover(|s| s.bg(t.danger).text_color(t.window_bg)),
    };
    styled.child(label.into())
}

/// Framed medium action button (`primary = true` fills with `t.accent`).
pub(super) fn bordered_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    button(
        id,
        label,
        if primary {
            ButtonVariant::Primary
        } else {
            ButtonVariant::Secondary
        },
        ButtonSize::Md,
        t,
    )
}

/// Signature Direction C panel/modal header with 6px accent marker, truncated title,
/// optional mono shortcut hint, and optional trailing action.
pub(in crate::ui) fn panel_header(
    title: impl Into<SharedString>,
    hint: Option<SharedString>,
    trailing: Option<AnyElement>,
    t: ThemeTokens,
) -> Div {
    div()
        .min_h(px((f32::from(t.control_h_md) + 10.).max(34.)))
        .flex_none()
        .px(px(SPACE_MD))
        .py(px(SPACE_XS))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(SPACE_SM))
        .bg(t.window_bg)
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(SPACE_SM))
                .min_w(px(0.))
                .child(div().size(px(6.)).flex_none().bg(t.accent))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_size(t.text_lg)
                        .text_color(t.text)
                        .truncate()
                        .child(title.into()),
                )
                .when_some(hint.filter(|h| !h.is_empty()), |d, hint| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(FONT_MONO)
                            .text_size(t.text_sm)
                            .text_color(t.text_faint)
                            .child(hint),
                    )
                }),
        )
        .when_some(trailing, |d, el| d.child(el))
}

/// Standard Direction C bordered card surface (`t.window_bg` + 1px `t.line`).
pub(in crate::ui) fn surface_card(gap: f32, t: ThemeTokens) -> Div {
    vstack(gap)
        .p(px(10.))
        .rounded(t.radius_md)
        .bg(t.window_bg)
        .border_1()
        .border_color(t.line)
}

/// Dim section heading label used above card groups and setting sections.
pub(in crate::ui) fn section_label(text: impl Into<SharedString>, t: ThemeTokens) -> Div {
    div()
        .text_size(t.text_sm)
        .text_color(t.text_dim)
        .child(text.into())
}

/// Horizontal tab strip container for sidebar mode and media filter switches.
pub(in crate::ui) fn tab_strip(t: ThemeTokens) -> Div {
    div()
        .min_h(px(f32::from(t.control_h_sm) + 6.))
        .flex_none()
        .px(px(6.))
        .py(px(3.))
        .flex()
        .items_center()
        .gap(px(2.))
        .border_b_1()
        .border_color(t.line)
}

/// Shrink-safe truncated tab item inside [`tab_strip`].
pub(in crate::ui) fn tab_strip_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    badge: Option<SharedString>,
    active: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .min_w(px(0.))
        .h(t.control_h_sm)
        .px(px(SPACE_XS))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(SPACE_XS))
        .rounded(t.radius_sm)
        .cursor_pointer()
        .text_size(t.text_sm)
        .when(active, |d| d.bg(t.selected).text_color(t.text))
        .when(!active, |d| {
            d.text_color(t.text_dim)
                .hover(|s| s.bg(t.hover).text_color(t.text))
        })
        .child(div().min_w(px(0.)).truncate().child(label.into()))
        .when_some(badge, |d, b| d.child(badge_pill(b, active, t)))
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
        .text_size(t.text_xs)
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
        div().pt(px(SPACE_SM)).px(px(SPACE_SM)).pb(px(28.)).child(
            div()
                .h(t.control_h_sm)
                .px(px(SPACE_SM))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(t.radius_sm)
                .bg(t.panel)
                .border_1()
                .border_color(t.line_strong)
                .font_family(FONT_UI)
                .text_size(t.text_md)
                .text_color(t.text)
                .whitespace_nowrap()
                .child(self.label.clone())
                .when_some(self.key.clone(), |d, key| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(t.text_sm)
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
