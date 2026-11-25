//! Unified virtual-list media/folder rows (`t.row_h`) and 3px slim scrollbar.

use gpui::{
    AnyElement, Div, ElementId, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, StyledImage,
    UniformListScrollHandle, div, prelude::FluentBuilder, px, relative,
};
use iris_core::library::Kind;

use super::{badge_pill, icon};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

const ROW_GROUP: &str = "media-list-row";

pub(in crate::ui) fn kind_glyph(kind: Kind) -> Icon {
    match kind {
        Kind::Video => Icon::Video,
        Kind::Photo => Icon::Photo,
        Kind::Audio => Icon::Music,
    }
}

/// Single-line selectable list row with unified right-side `t.accent` checkmark.
pub(in crate::ui) fn select_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    trailing_meta: Option<SharedString>,
    active: bool,
    kb_focused: bool,
    mono: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(t.row_h)
        .w_full()
        .flex_none()
        .px(px(10.))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .cursor_pointer()
        .text_size(t.text_md)
        .when(mono, |d| d.font_family(FONT_MONO))
        .when(kb_focused, |d| d.bg(t.hover).text_color(t.text))
        .when(active && !kb_focused, |d| d.bg(t.selected).text_color(t.text))
        .when(!active && !kb_focused, |d| {
            d.text_color(t.text_muted)
                .hover(|s| s.bg(t.hover).text_color(t.text))
        })
        .child(div().flex_1().min_w(px(0.)).truncate().child(label.into()))
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .when_some(trailing_meta.filter(|s| !s.is_empty()), |d, meta| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(t.text_xs)
                            .text_color(t.text_faint)
                            .child(meta),
                    )
                })
                .when(active, |d| d.child(icon(Icon::Check, 12., t.accent))),
        )
}

/// Renders a parent (`..`) or child folder row at `t.row_h`.
pub(in crate::ui) fn folder_row(
    id: impl Into<ElementId>,
    is_up: bool,
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    t: ThemeTokens,
) -> Stateful<Div> {
    let glyph = if is_up { Icon::ArrowUp } else { Icon::Folder };
    div()
        .id(id)
        .h(t.row_h)
        .w_full()
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(8.))
        .rounded(t.radius_sm)
        .cursor_pointer()
        .text_color(t.text_muted)
        .hover(|s| s.bg(t.hover).text_color(t.text))
        .active(|s| s.bg(t.selected))
        .child(icon(glyph, 14., if is_up { t.text_dim } else { t.accent }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(t.text_md)
                .child(title.into()),
        )
        .when_some(subtitle.filter(|s| !s.is_empty()), |d, sub| {
            d.child(
                div()
                    .flex_none()
                    .font_family(FONT_MONO)
                    .text_size(t.text_xs)
                    .text_color(t.text_faint)
                    .child(sub),
            )
        })
}

pub(in crate::ui) struct MediaRowSpec {
    pub(in crate::ui) kind: Kind,
    pub(in crate::ui) title: SharedString,
    pub(in crate::ui) meta: Option<SharedString>,
    pub(in crate::ui) is_current: bool,
    pub(in crate::ui) is_live: bool,
    pub(in crate::ui) progress: Option<f32>,
    pub(in crate::ui) trailing: Option<AnyElement>,
    pub(in crate::ui) thumbnail: Option<std::sync::Arc<gpui::RenderImage>>,
}

/// Renders a unified catalog or play-queue media row with optional 2px resume bar.
pub(in crate::ui) fn media_row(
    id: impl Into<ElementId>,
    spec: MediaRowSpec,
    t: ThemeTokens,
) -> Stateful<Div> {
    let glyph = if spec.is_current {
        Icon::Play
    } else {
        kind_glyph(spec.kind)
    };
    let glyph_color = if spec.is_current {
        t.accent
    } else {
        t.text_dim
    };
    let ratio = spec
        .progress
        .filter(|r| r.is_finite() && *r > 0.01)
        .map(|r| r.clamp(0.01, 1.0));

    div()
        .id(id)
        .group(ROW_GROUP)
        .relative()
        .h(t.row_h)
        .w_full()
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(8.))
        .rounded(t.radius_sm)
        .cursor_pointer()
        .when(spec.is_current, |d| d.bg(t.selected).text_color(t.text))
        .when(!spec.is_current, |d| {
            d.text_color(t.text_muted)
                .hover(|s| s.bg(t.hover).text_color(t.text))
        })
        .active(|s| s.bg(t.selected))
        .when(spec.is_current, |d| {
            d.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(4.))
                    .bottom(px(4.))
                    .w(px(2.))
                    .bg(t.accent),
            )
        })
        .child(match spec.thumbnail {
            Some(image) => gpui::img(image)
                .w(px(24.))
                .h(px(24.))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            None => icon(glyph, 14., glyph_color).into_any_element(),
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(t.text_md)
                .child(spec.title),
        )
        .when(spec.is_live, |d| d.child(badge_pill("LIVE", true, t)))
        .when_some(
            spec.meta.filter(|m| !m.is_empty() && !spec.is_live),
            |d, meta| {
                d.child(
                    div()
                        .flex_none()
                        .font_family(FONT_MONO)
                        .text_size(t.text_xs)
                        .text_color(t.text_faint)
                        .child(meta),
                )
            },
        )
        .when_some(spec.trailing, |d, el| d.child(el))
        .when_some(ratio, |d, r| {
            d.child(
                div()
                    .absolute()
                    .left(px(10.))
                    .right(px(10.))
                    .bottom(px(1.))
                    .h(px(2.))
                    .bg(t.line)
                    .child(div().h_full().w(relative(r)).bg(if spec.is_current {
                        t.accent
                    } else {
                        t.text_faint
                    })),
            )
        })
}

/// Trailing favorite star or remove button inside [`media_row`].
pub(in crate::ui) fn row_action_icon(
    id: impl Into<ElementId>,
    glyph: Icon,
    active_accent: bool,
    always_visible: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(20.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(t.radius_sm)
        .cursor_pointer()
        .when(!always_visible && !active_accent, |d| {
            d.opacity(0.).group_hover(ROW_GROUP, |s| s.opacity(1.))
        })
        .hover(|s| s.bg(t.panel))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(
            glyph,
            12.,
            if active_accent { t.accent } else { t.text_dim },
        ))
}

/// 3px slim right-aligned scrollbar indicator for a `UniformListScrollHandle`.
pub(in crate::ui) fn thin_scrollbar(
    handle: &UniformListScrollHandle,
    t: ThemeTokens,
) -> Option<Div> {
    let state = handle.0.borrow();
    let max_y = f32::from(state.base_handle.max_offset().height);
    let offset = f32::from(state.base_handle.offset().y);
    let view_h = f32::from(state.base_handle.bounds().size.height);
    scrollbar_geometry(view_h, max_y, offset).map(|(top, thumb_h)| {
        div()
            .absolute()
            .right(px(2.))
            .top(px(top))
            .w(px(3.))
            .h(px(thumb_h))
            .rounded(t.radius_sm)
            .bg(t.line_strong)
    })
}

fn scrollbar_geometry(view_h: f32, max_y: f32, offset: f32) -> Option<(f32, f32)> {
    if !view_h.is_finite()
        || !max_y.is_finite()
        || !offset.is_finite()
        || max_y <= 1.
        || view_h <= 20.
    {
        return None;
    }
    let max_thumb = view_h * 0.8;
    let thumb_h = (view_h * (view_h / (view_h + max_y))).clamp(24.0_f32.min(max_thumb), max_thumb);
    let top = (-offset).clamp(0., max_y) / max_y * (view_h - thumb_h);
    Some((top, thumb_h))
}

#[cfg(test)]
mod tests {
    use super::scrollbar_geometry;

    #[test]
    fn short_scroll_tracks_fit_the_thumb_at_both_ends() {
        for view in [20.1, 24., 29.9, 30., 120.] {
            let (top, height) = scrollbar_geometry(view, 100., 0.).unwrap();
            assert_eq!(top, 0.);
            assert!(height > 0. && height <= view * 0.8);
            let (bottom, _) = scrollbar_geometry(view, 100., -100.).unwrap();
            assert!(bottom + height <= view);
        }
        assert!(scrollbar_geometry(f32::NAN, 100., 0.).is_none());
    }
}
