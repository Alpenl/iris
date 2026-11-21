//! The directory panel (docked or fullscreen popup) and its collapsed rail.
use gpui::{
    Context, Div, InteractiveElement, IntoElement, MouseButton, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::*;

impl IrisWindow {
    /// The directory panel. Docked (`floating == false`) it is a column beside
    /// the video; floating it fills the fullscreen popup.
    pub(super) fn render_directory(
        &self,
        floating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let side = self.settings.directory_side;
        let videos = self.viewports[0]
            .session
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Video)
            .count();
        let photos = self.viewports[0].session.entries.len() - videos;
        // Four filters share the header with the side and collapse buttons.
        let tab = |label: String, filter: DirectoryFilter| {
            let active = self.filter == filter;
            div()
                .id(("filter", filter as usize))
                .h_full()
                .flex()
                .items_center()
                .px(px(5.))
                .text_size(px(11.))
                .when(active, |d| {
                    d.text_color(text()).border_b_2().border_color(accent())
                })
                .when(!active, |d| d.text_color(text_dim()))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    cx.notify();
                }))
                .child(label)
        };
        let items = self
            .display_entries()
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let is_current = self.viewports[0].session.current.as_ref() == Some(&e.path);
                let favorite = self.store.favorite(&e.path);
                let path = e.path.clone();
                div()
                    .id(("entry", i))
                    .h(px(48.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(line())
                    .cursor_pointer()
                    .hover(|s| s.bg(control()))
                    .when(is_current, |d| {
                        d.bg(accent_soft()).border_l_2().border_color(accent())
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.open_path(&path, cx)))
                    .child(
                        div()
                            .w(px(22.))
                            .font_family(FONT_MONO)
                            .text_size(px(10.))
                            .text_color(if is_current { accent() } else { text_dim() })
                            .child(format!("{:02}", i + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(12.)).truncate().child(if favorite {
                                format!("★ {}", e.name)
                            } else {
                                e.name.clone()
                            }))
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(9.))
                                    .text_color(if is_current { accent() } else { text_dim() })
                                    .child(if is_current {
                                        "▶ 播放中".to_string()
                                    } else if e.kind == Kind::Video {
                                        "VIDEO".to_string()
                                    } else {
                                        "PHOTO".to_string()
                                    }),
                            ),
                    )
            });
        // Two small panel icons choose which edge the directory sits on.
        let side_button = |id: &'static str, s: Side| {
            let active = side == s;
            div()
                .id(id)
                .w(px(26.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|st| st.bg(control()))
                .child(panel_icon(s, if active { accent() } else { text_dim() }))
                .on_click(cx.listener(move |this, _, _, cx| this.set_directory_side(s, cx)))
        };
        div()
            .w(px(DIRECTORY_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            // Opaque: a list over moving video is hard to read.
            .when(floating, |d| d.bg(panel()))
            .when(!floating, |d| {
                let d = d.bg(panel()).border_color(line());
                if side == Side::Left {
                    d.border_r_1()
                } else {
                    d.border_l_1()
                }
            })
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .border_b_1()
                    .border_color(line())
                    .child(tab("全部".into(), DirectoryFilter::All))
                    .child(tab(format!("视频 {videos}"), DirectoryFilter::Videos))
                    .child(tab(format!("照片 {photos}"), DirectoryFilter::Photos))
                    .child(tab("收藏".into(), DirectoryFilter::Favorites))
                    .child(div().flex_1())
                    .child(side_button("side-left", Side::Left))
                    .child(side_button("side-right", Side::Right))
                    .child(
                        div()
                            .id("collapse")
                            .w(px(36.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(text_dim())
                            .hover(|s| s.text_color(accent()))
                            .cursor_pointer()
                            // Points at the edge the panel folds into.
                            .child(if side == Side::Left { "⇤" } else { "⇥" })
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.toggle_directory(&ToggleDirectory, w, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("dir-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(items),
            )
    }

    pub(super) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let left = self.settings.directory_side == Side::Left;
        div()
            .w(px(RAIL_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .bg(panel())
            .border_color(line())
            .when(left, |d| d.border_r_1())
            .when(!left, |d| d.border_l_1())
            .child(
                div()
                    .id("expand")
                    .h(px(36.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_b_1()
                    .border_color(line())
                    .text_color(text_dim())
                    .hover(|s| s.text_color(accent()))
                    .cursor_pointer()
                    // Points the way the panel unfolds.
                    .child(if left { "⇥" } else { "⇤" })
                    .on_click(
                        cx.listener(|this, _, w, cx| {
                            this.toggle_directory(&ToggleDirectory, w, cx)
                        }),
                    ),
            )
            .child(
                div()
                    .pt(px(10.))
                    .font_family(FONT_MONO)
                    .text_size(px(9.))
                    .text_color(text_dim())
                    .child(format!("{}", self.viewports[0].session.entries.len())),
            )
    }
}

/// A 16x12 outline of a window with a filled bar on `side`: the directory icon.
pub(super) fn panel_icon(side: Side, color: Rgba) -> Div {
    div()
        .w(px(16.))
        .h(px(12.))
        .flex_none()
        .flex()
        .border_1()
        .border_color(color)
        .when(side == Side::Right, |d| d.justify_end())
        .child(div().w(px(5.)).h_full().bg(color))
}
