//! Windowed title bar: window buttons, then a drag area with the current
//! path on the left and the picture's size and decoder on the right.
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::*;

impl IrisWindow {
    pub(super) fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let file = session
            .current
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let folder = session
            .folder
            .as_ref()
            .and_then(|f| f.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未打开".into());
        let win_button = |id: &'static str| {
            div()
                .id(id)
                .size(px(10.))
                .border_1()
                .border_color(text_dim())
                .hover(|s| s.bg(accent()).border_color(accent()))
                .cursor_pointer()
        };
        div()
            .h(px(TITLEBAR_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(14.))
            .bg(window_bg())
            .border_b_1()
            .border_color(line())
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(win_button("win-close").on_click(|_, window, _| window.remove_window()))
                    .child(win_button("win-min").on_click(|_, window, _| window.minimize_window()))
                    .child(
                        win_button("win-full").on_click(|_, window, _| window.toggle_fullscreen()),
                    ),
            )
            .child(
                div()
                    .id("drag")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .pl(px(12.))
                    .pr(px(14.))
                    .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
                        if ev.click_count >= 2 {
                            window.titlebar_double_click();
                        } else {
                            platform::begin_window_drag(window);
                        }
                    })
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(13.))
                            .child("IRIS"),
                    )
                    .child(div().flex_none().w(px(1.)).h(px(14.)).bg(line()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .gap(px(6.))
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .child(div().flex_none().text_color(text_dim()).child(folder))
                            .when_some(file, |d, file| {
                                d.child(div().flex_none().text_color(text_faint()).child("/"))
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .truncate()
                                            .text_color(text())
                                            .child(file),
                                    )
                            }),
                    )
                    .when_some(self.media_info(), |d, info| {
                        d.child(
                            div()
                                .flex_none()
                                .font_family(FONT_MONO)
                                .text_size(px(11.))
                                .text_color(text_dim())
                                .child(info),
                        )
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
    }
}
