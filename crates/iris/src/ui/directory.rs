//! The directory panel: a column beside the player when windowed, a popup
//! over the picture in fullscreen. Hidden entirely when collapsed; the
//! control bar's toggle and Tab bring it back.
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::widgets::{icon, icon_button, tip};
use super::*;
use crate::assets::Icon;

impl IrisWindow {
    pub(super) fn render_directory(
        &self,
        floating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let side = self.settings.directory_side;
        let session = &self.viewports[0].session;
        let videos = session
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Video)
            .count();
        let photos = session.entries.len() - videos;
        let favorites = self.store.records.values().filter(|r| r.favorite).count();
        // Filters as text tabs; the current one is underlined in the accent,
        // on top of the header's bottom line.
        let tab = |label: &'static str, count: usize, filter: DirectoryFilter| {
            let active = self.filter == filter;
            div()
                .id(("filter", filter as usize))
                .relative()
                .h_full()
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if active { text() } else { text_dim() })
                .when(!active, |d| d.hover(|s| s.text_color(text())))
                .child(label)
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(if active { text_dim() } else { text_faint() })
                        .child(count.to_string()),
                )
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .right(px(8.))
                            .bottom(px(-1.))
                            .h(px(2.))
                            .bg(accent()),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    cx.notify();
                }))
        };
        let other = if side == Side::Left {
            Side::Right
        } else {
            Side::Left
        };
        // Keyed on the side: the button moves with the directory, and GPUI
        // would otherwise show a tooltip from before the move.
        let swap = icon_button(("swap-side", other as usize), Icon::Swap, false)
            .tooltip(tip(
                if other == Side::Right {
                    "目录移到右侧"
                } else {
                    "目录移到左侧"
                },
                None,
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.set_directory_side(other, cx)));

        let entries = self.display_entries();
        let note = entries.is_empty().then(|| self.directory_note());
        let rows = entries.into_iter().enumerate().map(|(i, e)| {
            let is_current = session.current.as_ref() == Some(&e.path);
            let favorite = self.store.favorite(&e.path);
            let path = e.path.clone();
            div()
                .id(("entry", i))
                .h(px(ROW_H))
                .flex_none()
                .mx(px(6.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(13.))
                .cursor_pointer()
                .when(is_current, |d| d.bg(selected()).text_color(text()))
                .when(!is_current, |d| {
                    d.text_color(text_muted())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .on_click(cx.listener(move |this, _, _, cx| this.open_path(&path, cx)))
                .child(icon(
                    if e.kind == Kind::Video {
                        Icon::Video
                    } else {
                        Icon::Photo
                    },
                    14.,
                    if is_current { accent() } else { text_faint() },
                ))
                .child(div().flex_1().min_w(px(0.)).truncate().child(e.name))
                .when(favorite, |d| {
                    d.child(icon(Icon::StarFill, 12., text_faint()))
                })
        });

        div()
            .w(px(DIRECTORY_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(panel())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .when(!floating, |d| {
                let d = d.border_color(line());
                if side == Side::Left {
                    d.border_r_1()
                } else {
                    d.border_l_1()
                }
            })
            .child(
                div()
                    .h(px(HEADER_H))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(6.))
                    .border_b_1()
                    .border_color(line())
                    .child(tab("全部", session.entries.len(), DirectoryFilter::All))
                    .child(tab("视频", videos, DirectoryFilter::Videos))
                    .child(tab("照片", photos, DirectoryFilter::Photos))
                    .child(tab("收藏", favorites, DirectoryFilter::Favorites))
                    .child(div().flex_1())
                    .child(swap),
            )
            .child(
                div()
                    .id("dir-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .py(px(6.))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .children(note),
            )
    }

    /// Why the list is empty, and what to do about it.
    fn directory_note(&self) -> impl IntoElement {
        let (title, hint) = match self.filter {
            DirectoryFilter::Favorites => ("还没有收藏", Some("按 S 收藏正在看的文件")),
            _ if self.viewports[0].session.folder.is_none() => {
                ("未打开文件夹", Some("把文件或文件夹拖进窗口"))
            }
            DirectoryFilter::Videos => ("这个文件夹里没有视频", None),
            DirectoryFilter::Photos => ("这个文件夹里没有照片", None),
            DirectoryFilter::All => ("这个文件夹里没有能打开的文件", None),
        };
        div()
            .px(px(14.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(div().text_size(px(12.)).text_color(text_dim()).child(title))
            .when_some(hint, |d, hint| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(text_faint())
                        .child(hint),
                )
            })
    }
}
