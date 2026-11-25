//! Centered modal dialog shell (`t.radius_lg`, scrim click/scroll isolation)
//! and popup/context-menu row primitives.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, ElementId, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};

use super::super::IrisWindow;
use super::{bordered_button, icon, icon_button, tip};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

pub(in crate::ui) type ModalCallback =
    Rc<dyn Fn(&mut IrisWindow, &mut Window, &mut Context<IrisWindow>)>;

pub(in crate::ui) struct ModalFooterSpec {
    pub(in crate::ui) cancel_label: SharedString,
    pub(in crate::ui) submit_label: SharedString,
    pub(in crate::ui) submit_disabled: bool,
    pub(in crate::ui) on_submit: ModalCallback,
}

pub(in crate::ui) struct ModalShellSpec {
    pub(in crate::ui) id_prefix: &'static str,
    pub(in crate::ui) title: SharedString,
    pub(in crate::ui) header_hint: Option<SharedString>,
    pub(in crate::ui) error: Option<SharedString>,
    pub(in crate::ui) width: f32,
    pub(in crate::ui) fixed_height: Option<f32>,
    pub(in crate::ui) max_height: f32,
    pub(in crate::ui) raw_body: bool,
    pub(in crate::ui) accent_border: bool,
    pub(in crate::ui) body: AnyElement,
    pub(in crate::ui) on_cancel: ModalCallback,
    pub(in crate::ui) footer: Option<ModalFooterSpec>,
}

impl ModalShellSpec {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ui) fn dialog(
        id_prefix: &'static str,
        title: impl Into<SharedString>,
        width: f32,
        max_height: f32,
        cancel_label: impl Into<SharedString>,
        submit_label: impl Into<SharedString>,
        body: AnyElement,
        on_cancel: ModalCallback,
        on_submit: ModalCallback,
    ) -> Self {
        Self {
            id_prefix,
            title: title.into(),
            header_hint: None,
            error: None,
            width,
            fixed_height: None,
            max_height,
            raw_body: false,
            accent_border: false,
            body,
            on_cancel,
            footer: Some(ModalFooterSpec {
                cancel_label: cancel_label.into(),
                submit_label: submit_label.into(),
                submit_disabled: false,
                on_submit,
            }),
        }
    }

    pub(in crate::ui) fn with_error(mut self, error: Option<SharedString>) -> Self {
        self.error = error;
        self
    }

    pub(in crate::ui) fn with_submit_disabled(mut self, disabled: bool) -> Self {
        if let Some(footer) = &mut self.footer {
            footer.submit_disabled = disabled;
        }
        self
    }
}

/// Centered modal shell with scrim backdrop that stops click and wheel propagation.
pub(in crate::ui) fn modal_shell(
    spec: ModalShellSpec,
    t: ThemeTokens,
    cx: &mut Context<IrisWindow>,
) -> AnyElement {
    let (cancel_bd, cancel_x, cancel_btn) = (
        spec.on_cancel.clone(),
        spec.on_cancel.clone(),
        spec.on_cancel,
    );

    let header = div()
        .h(px(38.))
        .flex_none()
        .px(px(14.))
        .flex()
        .items_center()
        .justify_between()
        .bg(t.window_bg)
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .min_w(px(0.))
                .child(div().size(px(6.)).flex_none().bg(t.accent))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_size(px(13.))
                        .text_color(t.text)
                        .truncate()
                        .child(spec.title),
                )
                .when_some(spec.header_hint.filter(|h| !h.is_empty()), |d, hint| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(t.text_faint)
                            .child(hint),
                    )
                }),
        )
        .child(
            icon_button((spec.id_prefix, 2usize), Icon::Close, false, t)
                .tooltip(tip("关闭", Some("Esc".into())))
                .on_click(cx.listener(move |this, _, w, cx| cancel_x(this, w, cx))),
        );

    let error_banner = spec.error.filter(|msg| !msg.trim().is_empty()).map(|msg| {
        div()
            .flex_none()
            .px(px(14.))
            .py(px(8.))
            .bg(t.window_bg)
            .border_t_1()
            .border_color(t.danger)
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().flex_none().size(px(6.)).bg(t.danger))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(11.))
                    .text_color(t.danger)
                    .child(msg),
            )
    });

    let footer = spec.footer.map(|f| {
        let submit_btn = f.on_submit;
        let submit_el = if f.submit_disabled {
            bordered_button((spec.id_prefix, 5usize), f.submit_label, true, t)
                .opacity(0.45)
                .cursor_default()
        } else {
            bordered_button((spec.id_prefix, 5usize), f.submit_label, true, t)
                .on_click(cx.listener(move |this, _, w, cx| submit_btn(this, w, cx)))
        };
        div()
            .min_h(px(44.))
            .py(px(8.))
            .flex_none()
            .px(px(14.))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.))
            .bg(t.window_bg)
            .border_t_1()
            .border_color(t.line)
            .child(
                bordered_button((spec.id_prefix, 4usize), f.cancel_label, false, t)
                    .on_click(cx.listener(move |this, _, w, cx| cancel_btn(this, w, cx))),
            )
            .child(submit_el)
    });

    let body_wrap = if spec.raw_body {
        div()
            .id((spec.id_prefix, 3usize))
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(spec.body)
    } else {
        div()
            .id((spec.id_prefix, 3usize))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(px(14.))
            .child(spec.body)
    };

    div()
        .id((spec.id_prefix, 0usize))
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(t.scrim)
        .p(px(12.))
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, w, cx| {
                cancel_bd(this, w, cx);
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(
            div()
                .id((spec.id_prefix, 1usize))
                .w(px(spec.width))
                .max_w(gpui::relative(1.))
                .when_some(spec.fixed_height, |d, h| d.h(px(h)))
                .when(spec.fixed_height.is_some(), |d| d.max_h(gpui::relative(1.)))
                .when(spec.fixed_height.is_none(), |d| {
                    d.max_h(px(spec.max_height))
                })
                .flex()
                .flex_col()
                .rounded(t.radius_lg)
                .bg(t.panel)
                .border_1()
                .border_color(if spec.accent_border {
                    t.accent
                } else {
                    t.line_strong
                })
                .overflow_hidden()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(header)
                .child(body_wrap)
                .children(error_banner)
                .children(footer),
        )
        .into_any_element()
}

pub(in crate::ui) fn menu_row(
    id: impl Into<ElementId>,
    glyph: Option<Icon>,
    label: impl Into<SharedString>,
    shortcut: Option<SharedString>,
    active_accent: bool,
    t: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(t.row_h)
        .w_full()
        .px(px(10.))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .rounded(t.radius_sm)
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(if active_accent { t.accent } else { t.text })
        .hover(|s| s.bg(t.hover))
        .active(|s| s.bg(t.selected))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .min_w(px(0.))
                .when_some(glyph, |d, g| {
                    d.child(icon(
                        g,
                        14.,
                        if active_accent { t.accent } else { t.text_dim },
                    ))
                })
                .child(div().truncate().child(label.into())),
        )
        .when_some(shortcut.filter(|s| !s.is_empty()), |d, key| {
            d.child(
                div()
                    .flex_none()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(t.text_dim)
                    .child(key),
            )
        })
}

pub(in crate::ui) fn menu_divider(t: ThemeTokens) -> Div {
    div().h(px(1.)).my(px(4.)).bg(t.line)
}
