//! Small pieces shared by the control bar, the directory and the overlays:
//! icons, ghost buttons, separators and tooltips.
use gpui::{
    AnyView, App, AppContext as _, Context, Div, ElementId, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Svg, Window,
    div, prelude::FluentBuilder, px, svg,
};

use crate::assets::Icon;
use crate::theme::*;

/// Group name every ghost button registers, so its icon can brighten on hover.
const GHOST: &str = "ghost";

pub(super) fn icon(glyph: Icon, size: f32, color: gpui::Rgba) -> Svg {
    svg()
        .path(glyph.path())
        .size(px(size))
        .flex_none()
        .text_color(color)
}

/// A button with no frame at rest and a filled square on hover; `on` marks a
/// switched-on toggle. Text inside is dim until hovered. Callers add the
/// content, tooltip and click handler.
pub(super) fn ghost(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .group(GHOST)
        .h(px(BUTTON))
        .min_w(px(BUTTON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(on, |d| d.bg(selected()).text_color(text()))
        .when(!on, |d| {
            d.text_color(text_dim())
                .hover(|s| s.bg(hover()).text_color(text()))
        })
        .active(|s| s.bg(selected()))
}

/// A square ghost button holding one 16px icon.
pub(super) fn icon_button(id: impl Into<ElementId>, glyph: Icon, on: bool) -> Stateful<Div> {
    ghost(id, on).child(icon_on_ghost(glyph, if on { text() } else { text_dim() }))
}

/// An icon that turns to full text colour while its ghost button is hovered.
pub(super) fn icon_on_ghost(glyph: Icon, color: gpui::Rgba) -> Svg {
    icon(glyph, 16., color).group_hover(GHOST, |s| s.text_color(text()))
}

/// A ghost button labelled in the mono face (`1:1`, `1.00×`).
pub(super) fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    ghost(id, false)
        .px(px(6.))
        .font_family(FONT_MONO)
        .text_size(px(11.))
        .child(label.into())
}

/// A 1px divider between groups of controls.
pub(super) fn separator() -> Div {
    div().flex_none().w(px(1.)).h(px(16.)).mx(px(6.)).bg(line())
}

/// Tooltip content: a label and, when there is one, the key that does the same.
struct Tip {
    label: SharedString,
    key: Option<&'static str>,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // GPUI places the tooltip 1px from the pointer, flipping it above the
        // pointer near the bottom of the window. The transparent margin keeps
        // the box off the cursor; the deeper bottom margin lifts a flipped
        // tooltip clear of the control bar and its timeline.
        div().pt(px(8.)).px(px(8.)).pb(px(28.)).child(
            div()
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(10.))
                .bg(panel())
                .border_1()
                .border_color(line_strong())
                .font_family(FONT_UI)
                .text_size(px(12.))
                .text_color(text())
                .whitespace_nowrap()
                .child(self.label.clone())
                .when_some(self.key, |d, key| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(text_dim())
                            .child(key),
                    )
                }),
        )
    }
}

pub(super) fn tip(
    label: impl Into<SharedString>,
    key: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let label = label.into();
    move |_, cx| {
        cx.new(|_| Tip {
            label: label.clone(),
            key,
        })
        .into()
    }
}
