//! The directory panel: a resizable column beside the player when windowed, a
//! popup over the picture in fullscreen. Hidden entirely when collapsed; the
//! control bar's toggle and Tab bring it back.
use gpui::{
    Context, CursorStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
    relative,
};

use super::widgets::{icon, icon_button, tip};
use super::*;
use crate::assets::Icon;
use crate::settings::{DEFAULT_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH};

impl IrisWindow {
    /// Clamps the saved directory width so the picture always keeps room on
    /// narrow windows.
    pub(super) fn effective_directory_width(&self, window: &Window) -> f32 {
        let vw = f32::from(window.viewport_size().width);
        let max = if vw > 0. {
            (vw * 0.45).clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
        } else {
            MAX_DIRECTORY_WIDTH
        };
        self.settings
            .directory_width
            .clamp(MIN_DIRECTORY_WIDTH, max)
    }

    pub(super) fn render_directory(
        &self,
        floating: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let side = self.settings.directory_side;
        let width = self.effective_directory_width(window);
        let compact = width < 276.;
        let session = &self.viewports[0].session;
        let videos = session
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Video)
            .count();
        let photos = session.entries.len() - videos;
        let favorites = self.store.records.values().filter(|r| r.favorite).count();
        let open_key = if cfg!(target_os = "macos") {
            "⌘O"
        } else {
            "Ctrl+O"
        };

        // Filters as text tabs; the current one is underlined in the accent,
        // on top of the header's bottom line.
        let tab = |label: &'static str, count: usize, filter: DirectoryFilter| {
            let active = self.filter == filter;
            div()
                .id(("filter", filter as usize))
                .relative()
                .h_full()
                .px(px(if compact { 5. } else { 7. }))
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if active { text() } else { text_dim() })
                .when(!active, |d| d.hover(|s| s.text_color(text())))
                .child(label)
                .when(!compact, |d| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(10.))
                            .text_color(if active { text_dim() } else { text_faint() })
                            .child(count.to_string()),
                    )
                })
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(5.))
                            .right(px(5.))
                            .bottom(px(-1.))
                            .h(px(2.))
                            .bg(accent()),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    this.scroll_directory_to_current();
                    cx.notify();
                }))
        };

        let other = if side == Side::Left {
            Side::Right
        } else {
            Side::Left
        };
        let open_btn = icon_button("dir-open", Icon::FolderOpen, false)
            .tooltip(tip("打开文件或文件夹", Some(open_key)))
            .on_click(cx.listener(|this, _, w, cx| this.open_file(&OpenFile, w, cx)));
        let refresh_btn = session.folder.is_some().then(|| {
            icon_button("dir-refresh", Icon::Refresh, false)
                .tooltip(tip(
                    "刷新目录",
                    Some(if cfg!(target_os = "macos") {
                        "⌘R"
                    } else {
                        "Ctrl+R"
                    }),
                ))
                .on_click(
                    cx.listener(|this, _, w, cx| this.refresh_directory(&RefreshDirectory, w, cx)),
                )
        });
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
        let in_favorites = self.filter == DirectoryFilter::Favorites;
        let rows = entries.into_iter().enumerate().map(|(i, e)| {
            let is_current = session.current.as_ref() == Some(&e.path);
            let favorite = self.store.favorite(&e.path);
            let progress = (e.kind == Kind::Video)
                .then(|| self.store.progress_ratio(&e.path))
                .flatten();
            let path = e.path.clone();
            let remove_path = e.path.clone();
            div()
                .id(("entry", i))
                .group("dir-row")
                .relative()
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
                .when(in_favorites, |d| {
                    d.child(
                        div()
                            .id(("unfav", i))
                            .size(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .opacity(0.)
                            .group_hover("dir-row", |s| s.opacity(1.))
                            .hover(|s| s.bg(selected()))
                            .tooltip(tip("移出收藏", None))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.store.remove_favorite(&remove_path) {
                                    this.show_toast("已移出收藏", cx);
                                }
                            }))
                            .child(icon(Icon::Close, 10., text_dim())),
                    )
                })
                .when(!in_favorites && favorite, |d| {
                    d.child(icon(Icon::StarFill, 12., text_faint()))
                })
                .when_some(progress, |d, ratio| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .right(px(8.))
                            .bottom(px(1.))
                            .h(px(2.))
                            .bg(line())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(ratio))
                                    .bg(if is_current { accent() } else { text_faint() }),
                            ),
                    )
                })
        });

        let resizing = self.resizing_dir.is_some();
        let resize_handle = (!floating).then(|| {
            let edge = div()
                .id("dir-resize")
                .group("dir-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(6.))
                .cursor(CursorStyle::ResizeLeftRight)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        if ev.click_count >= 2 {
                            this.settings.directory_width = DEFAULT_DIRECTORY_WIDTH;
                            this.settings.save();
                            this.resizing_dir = None;
                            this.show_toast("目录宽度已重置", cx);
                        } else {
                            let w = this.effective_directory_width(window);
                            this.resizing_dir = Some((f32::from(ev.position.x), w));
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .w(px(1.))
                        .when(side == Side::Left, |d| d.right_0())
                        .when(side == Side::Right, |d| d.left_0())
                        .when(resizing, |d| d.bg(accent()))
                        .when(!resizing, |d| {
                            d.bg(line()).group_hover("dir-resize", |s| s.bg(accent()))
                        }),
                );
            if side == Side::Left {
                edge.right_0()
            } else {
                edge.left_0()
            }
        });

        div()
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(panel())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(HEADER_H))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(4.))
                    .border_b_1()
                    .border_color(line())
                    .child(tab("全部", session.entries.len(), DirectoryFilter::All))
                    .child(tab("视频", videos, DirectoryFilter::Videos))
                    .child(tab("照片", photos, DirectoryFilter::Photos))
                    .child(tab("收藏", favorites, DirectoryFilter::Favorites))
                    .child(div().flex_1())
                    .child(open_btn)
                    .children(refresh_btn)
                    .child(swap),
            )
            .child(
                div()
                    .id("dir-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.dir_scroll)
                    .py(px(6.))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .children(note),
            )
            .children(resize_handle)
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
