//! Windowed title bar: window buttons, drag area and the current path.
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, px,
};

use super::*;

impl IrisWindow {
    pub(super) fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file = self.viewports[0]
            .session
            .current
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let folder = self.viewports[0]
            .session
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
            .px(px(14.))
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
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .pl(px(12.))
                    .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
                        if ev.click_count >= 2 {
                            window.titlebar_double_click();
                        } else {
                            platform::begin_window_drag(window);
                        }
                    })
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(13.))
                            .child("IRIS"),
                    )
                    .child(div().w(px(1.)).h(px(14.)).bg(line()))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .child(div().text_color(text_dim()).child(folder))
                            .child(div().text_color(text_dim()).child("/"))
                            .child(div().text_color(text()).child(file)),
                    ),
            )
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(text_dim())
                    .px(px(6.))
                    .py(px(2.))
                    .border_1()
                    .border_color(line())
                    .child("SPIKE · GPUI 0.2.2 + libmpv"),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
    }
}
