//! Windowed title bar: platform-conventional window buttons (left on macOS,
//! right on Windows), drag area with current path, media info, and pin toggle.
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::widgets::{icon, tip};
use super::*;
use crate::assets::Icon;

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
        let pinned = self.settings.always_on_top;

        let win_btn = |id: &'static str, glyph: Icon, label: &'static str, accent_hover: bool| {
            let group_name = id;
            div()
                .id(id)
                .group(group_name)
                .size(px(22.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(line())
                .cursor_pointer()
                .tooltip(tip(label, None))
                .when(accent_hover, |d| {
                    d.hover(|s| s.bg(accent()).border_color(accent()))
                })
                .when(!accent_hover, |d| {
                    d.hover(|s| s.bg(hover()).border_color(line_strong()))
                })
                .child(
                    icon(glyph, 11., text_dim()).group_hover(group_name, |s| s.text_color(text())),
                )
        };

        let pin_btn = div()
            .id(("win-pin", usize::from(pinned)))
            .group("win-pin")
            .size(px(22.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(if pinned { accent() } else { line() })
            .when(pinned, |d| d.bg(selected()))
            .when(!pinned, |d| {
                d.hover(|s| s.bg(hover()).border_color(line_strong()))
            })
            .cursor_pointer()
            .tooltip(tip(
                if pinned { "取消窗口置顶" } else { "窗口置顶" },
                Some("T"),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, w, cx| this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx)))
            .child(
                icon(Icon::Pin, 12., if pinned { accent() } else { text_dim() })
                    .group_hover("win-pin", |s| {
                        s.text_color(if pinned { accent() } else { text() })
                    }),
            );

        let mac_buttons = || {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(10.))
                .child(
                    win_btn("win-close", Icon::Close, "关闭", true)
                        .on_click(|_, window, _| window.remove_window()),
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
                        .on_click(|_, window, _| window.zoom_window()),
                )
                .child(
                    win_btn("win-close", Icon::Close, "关闭", true)
                        .on_click(|_, window, _| window.remove_window()),
                )
        };

        let drag_area = div()
            .id("drag")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_center()
            .gap(px(10.))
            .pl(px(if cfg!(target_os = "macos") { 12. } else { 14. }))
            .pr(px(10.))
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
            })
            .child(pin_btn);

        let bar = div()
            .h(px(TITLEBAR_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .bg(window_bg())
            .border_b_1()
            .border_color(line())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            );
        if cfg!(target_os = "macos") {
            bar.child(mac_buttons()).child(drag_area)
        } else {
            bar.child(drag_area).child(win_buttons())
        }
    }
}
