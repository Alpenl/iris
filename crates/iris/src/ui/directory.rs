//! The directory panel: a resizable, virtualized column beside the player when
//! windowed, a popup over the picture in fullscreen. Supports search, sorting,
//! subfolder navigation, progress bars, and right-click context menus.
use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, CursorStyle, ElementInputHandler, InteractiveElement, IntoElement,
    KeyContext, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder, px, relative,
    uniform_list,
};

use super::context_menu::{ContextMenu, ContextTarget};
use super::widgets::{icon, icon_button, tip};
use super::*;
use crate::assets::Icon;
use crate::library::{FolderEntry, format_size};
use crate::settings::{DEFAULT_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH};

#[derive(Clone, Debug)]
pub(super) enum DirRow {
    Parent(PathBuf),
    Folder(FolderEntry),
    Media(Entry),
}

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

    /// Rebuilds the cached sorted entries, photo slice, tab counts, and filtered
    /// row list whenever entries, filter, search, sort, or favorites change.
    pub(super) fn rebuild_directory_rows(&mut self) {
        let query = self.search_input.text().trim().to_ascii_lowercase();
        let sorted = self.compute_sorted_entries();
        let session = &self.active().session;
        let video_audio_count = session
            .entries
            .iter()
            .filter(|e| matches!(e.kind, Kind::Video | Kind::Audio))
            .count();
        let photo_count = session
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Photo)
            .count();
        let favorite_count = self.store.records.values().filter(|r| r.favorite).count();
        let photo_entries: Vec<Entry> = sorted
            .iter()
            .filter(|e| e.kind == Kind::Photo)
            .cloned()
            .collect();

        let mut rows = Vec::new();
        // Show parent & subfolder navigation in folder views (not Favorites).
        if self.filter != DirectoryFilter::Favorites
            && let Some(folder) = &session.folder
        {
            if query.is_empty()
                && let Some(parent) = folder.parent()
                && parent != folder.as_path()
            {
                rows.push(DirRow::Parent(parent.to_path_buf()));
            }
            for sub in &session.subfolders {
                if query.is_empty() || sub.name.to_ascii_lowercase().contains(&query) {
                    rows.push(DirRow::Folder(sub.clone()));
                }
            }
        }

        for e in &sorted {
            if query.is_empty() || e.name.to_ascii_lowercase().contains(&query) {
                rows.push(DirRow::Media(e.clone()));
            }
        }
        self.dir_cache = DirectoryCache {
            rows,
            sorted_entries: sorted,
            photo_entries,
            video_audio_count,
            photo_count,
            favorite_count,
        };
    }

    pub(super) fn render_directory(
        &self,
        floating: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let side = self.settings.directory_side;
        let width = self.effective_directory_width(window);
        let compact = width < 320.;
        let plugin_body = self
            .sidebar_plugins
            .then(|| self.render_plugin_sidebar(cx).into_any_element());
        let session = &self.active().session;
        let videos = self.dir_cache.video_audio_count;
        let photos = self.dir_cache.photo_count;
        let favorites = self.dir_cache.favorite_count;
        let open_key = if cfg!(target_os = "macos") {
            "⌘O"
        } else {
            "Ctrl+O"
        };
        let search_key = if cfg!(target_os = "macos") {
            "⌘F"
        } else {
            "Ctrl+F"
        };

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
                    this.rebuild_directory_rows();
                    this.scroll_directory_to_current();
                    cx.notify();
                }))
        };

        let other = if side == Side::Left {
            Side::Right
        } else {
            Side::Left
        };
        let sort_label = format!(
            "排序 · {} {}",
            match self.settings.sort_by {
                SortBy::Name => "名称",
                SortBy::Modified => "时间",
                SortBy::Size => "大小",
            },
            match self.settings.sort_order {
                SortOrder::Asc => "↑",
                SortOrder::Desc => "↓",
            }
        );

        let search_btn = icon_button(
            ("dir-search", usize::from(self.search_open)),
            Icon::Search,
            self.search_open || !self.search_input.is_empty(),
        )
        .tooltip(tip("搜索目录", Some(search_key)))
        .on_click(cx.listener(|this, _, w, cx| this.toggle_search(&ToggleSearch, w, cx)));

        let sort_btn = icon_button(
            (
                "dir-sort",
                self.settings.sort_by as usize * 2 + self.settings.sort_order as usize,
            ),
            Icon::Sort,
            self.settings.sort_by != SortBy::Name || self.settings.sort_order != SortOrder::Asc,
        )
        .tooltip(tip(sort_label, None))
        .on_click(cx.listener(|this, _, _, cx| this.cycle_sort(cx)));

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

        let plugins_btn = icon_button(
            ("dir-plugins", usize::from(self.sidebar_plugins)),
            Icon::Puzzle,
            self.sidebar_plugins,
        )
        .tooltip(tip(
            if self.sidebar_plugins {
                "返回文件目录"
            } else {
                "切换到插件面板"
            },
            None,
        ))
        .on_click(cx.listener(|this, _, _, cx| {
            this.sidebar_plugins = !this.sidebar_plugins;
            cx.notify();
        }));

        let row_count = self.dir_cache.rows.len();
        let note = (row_count == 0).then(|| self.directory_note());
        let entity = cx.entity().clone();

        // Subtle right-edge scrollbar indicator derived from UniformListScrollHandle.
        let scrollbar = {
            let state = self.dir_scroll.0.borrow();
            let max_y = f32::from(state.base_handle.max_offset().height);
            let off_y = (-f32::from(state.base_handle.offset().y)).clamp(0., max_y.max(0.));
            let view_h = f32::from(state.base_handle.bounds().size.height);
            (max_y > 1. && view_h > 20.).then(|| {
                let total = view_h + max_y;
                let thumb_h = (view_h * (view_h / total)).clamp(24., view_h * 0.8);
                let top = (off_y / max_y) * (view_h - thumb_h);
                div()
                    .absolute()
                    .right(px(2.))
                    .top(px(top))
                    .w(px(3.))
                    .h(px(thumb_h))
                    .bg(line_strong())
            })
        };

        let list = uniform_list("dir-list", row_count, move |range, _window, cx| {
            let this = entity.read(cx);
            let session = &this.active().session;
            let in_favorites = this.filter == DirectoryFilter::Favorites;
            range
                .map(|i| {
                    let row = &this.dir_cache.rows[i];
                    match row {
                        DirRow::Parent(parent) => {
                            let p = parent.clone();
                            let entity = entity.clone();
                            div()
                                .id(("entry", i))
                                .h(px(ROW_H))
                                .mx(px(6.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(12.))
                                .text_color(text_dim())
                                .cursor_pointer()
                                .hover(|s| s.bg(hover()).text_color(text()))
                                .tooltip(tip("返回上一级目录", None))
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.open_path(&p, cx));
                                })
                                .child(icon(Icon::ArrowUp, 13., text_faint()))
                                .child("..")
                                .into_any_element()
                        }
                        DirRow::Folder(sub) => {
                            let p = sub.path.clone();
                            let name = sub.name.clone();
                            let entity = entity.clone();
                            div()
                                .id(("entry", i))
                                .h(px(ROW_H))
                                .mx(px(6.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(13.))
                                .text_color(text_dim())
                                .cursor_pointer()
                                .hover(|s| s.bg(hover()).text_color(text()))
                                .tooltip(tip(name.clone(), None))
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.open_path(&p, cx));
                                })
                                .child(icon(Icon::Folder, 14., text_faint()))
                                .child(div().flex_1().min_w(px(0.)).truncate().child(name))
                                .into_any_element()
                        }
                        DirRow::Media(e) => {
                            let is_current = session.current.as_ref() == Some(&e.path);
                            let favorite = this.store.favorite(&e.path);
                            let progress = matches!(e.kind, Kind::Video | Kind::Audio)
                                .then(|| this.store.progress_ratio(&e.path))
                                .flatten();
                            let path = e.path.clone();
                            let ctx_path = e.path.clone();
                            let remove_path = e.path.clone();
                            let kind = e.kind;
                            let tip_text = if e.size > 0 {
                                format!("{} · {}", e.name, format_size(e.size))
                            } else {
                                e.name.clone()
                            };
                            let glyph = match e.kind {
                                Kind::Video => Icon::Video,
                                Kind::Photo => Icon::Photo,
                                Kind::Audio => Icon::Music,
                            };
                            let e_open = entity.clone();
                            let e_ctx = entity.clone();
                            let e_rm = entity.clone();
                            div()
                                .id(("entry", i))
                                .group("dir-row")
                                .relative()
                                .h(px(ROW_H))
                                .mx(px(6.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .text_size(px(13.))
                                .cursor_pointer()
                                .tooltip(tip(tip_text, None))
                                .when(is_current, |d| d.bg(selected()).text_color(text()))
                                .when(!is_current, |d| {
                                    d.text_color(text_muted())
                                        .hover(|s| s.bg(hover()).text_color(text()))
                                })
                                .on_click(move |_, _, cx| {
                                    e_open.update(cx, |this, cx| this.open_path(&path, cx));
                                })
                                .on_mouse_down(
                                    MouseButton::Right,
                                    move |ev: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        let pos = ev.position;
                                        let p = ctx_path.clone();
                                        e_ctx.update(cx, |this, cx| {
                                            this.overlays.open_context_menu(ContextMenu {
                                                pos,
                                                target: ContextTarget::Entry { path: p, kind },
                                            });
                                            cx.notify();
                                        });
                                    },
                                )
                                .child(icon(
                                    glyph,
                                    14.,
                                    if is_current { accent() } else { text_faint() },
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .child(e.name.clone()),
                                )
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
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_click(move |_, _, cx| {
                                                e_rm.update(cx, |this, cx| {
                                                    if this.store.remove_favorite(&remove_path) {
                                                        this.rebuild_directory_rows();
                                                        this.show_toast("已移出收藏", cx);
                                                    }
                                                });
                                            })
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
                                            .child(div().h_full().w(relative(ratio)).bg(
                                                if is_current { accent() } else { text_faint() },
                                            )),
                                    )
                                })
                                .into_any_element()
                        }
                    }
                })
                .collect()
        })
        .track_scroll(self.dir_scroll.clone())
        .size_full();

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

        let search_bar = self.search_open.then(|| {
            let focused = self.search_focus.is_focused(window);
            let is_empty = self.search_input.is_empty();
            let display = if is_empty {
                "输入关键字过滤…".to_string()
            } else {
                self.search_input.display_text(focused)
            };
            let focus_ime = self.search_focus.clone();
            let entity_ime = cx.entity().clone();
            div()
                .id("dir-search-bar")
                .relative()
                .h(px(32.))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .bg(window_bg())
                .border_b_1()
                .border_color(if focused { accent() } else { line() })
                .track_focus(&self.search_focus)
                .key_context({
                    let mut ctx = KeyContext::default();
                    ctx.add("SearchInput");
                    ctx.add("TextInput");
                    ctx
                })
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, cx| {
                            window.handle_input(
                                &focus_ime,
                                ElementInputHandler::new(bounds, entity_ime.clone()),
                                cx,
                            );
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.search_focus.focus(window);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if this.search_input.marked_range.is_some() {
                        // Active IME composition: let the OS input method handle editing/confirm/cancel keys.
                        return;
                    }
                    let mods = ev.keystroke.modifiers;
                    if mods.secondary() && !mods.alt {
                        match ev.keystroke.key.as_str() {
                            "a" => {
                                this.search_input.select_all();
                                cx.stop_propagation();
                                cx.notify();
                                return;
                            }
                            "c" => {
                                let sel = this.search_input.selected_text();
                                let to_copy = if sel.is_empty() {
                                    this.search_input.text()
                                } else {
                                    sel
                                };
                                if !to_copy.is_empty() {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        to_copy.to_string(),
                                    ));
                                }
                                cx.stop_propagation();
                                return;
                            }
                            "x" => {
                                let sel = this.search_input.selected_text().to_string();
                                if !sel.is_empty() {
                                    cx.write_to_clipboard(ClipboardItem::new_string(sel));
                                    this.search_input.backspace();
                                    this.rebuild_directory_rows();
                                    cx.notify();
                                }
                                cx.stop_propagation();
                                return;
                            }
                            "v" => {
                                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text())
                                {
                                    this.search_input.insert_paste(&text);
                                    this.rebuild_directory_rows();
                                    cx.notify();
                                }
                                cx.stop_propagation();
                                return;
                            }
                            _ => return,
                        }
                    }
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.search_input.clear();
                            this.search_open = false;
                            this.rebuild_directory_rows();
                            this.focus.focus(window);
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "backspace" => {
                            this.search_input.backspace();
                            this.rebuild_directory_rows();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "delete" => {
                            this.search_input.delete_forward();
                            this.rebuild_directory_rows();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "left" => {
                            this.search_input.move_left();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "right" => {
                            this.search_input.move_right();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "home" => {
                            this.search_input.move_home();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "end" => {
                            this.search_input.move_end();
                            cx.stop_propagation();
                            cx.notify();
                        }
                        "enter" => {
                            if let Some(DirRow::Media(e)) = this
                                .dir_cache
                                .rows
                                .iter()
                                .find(|r| matches!(r, DirRow::Media(_)))
                                .cloned()
                            {
                                this.open_path(&e.path, cx);
                            }
                            this.focus.focus(window);
                            cx.stop_propagation();
                            cx.notify();
                        }
                        _ => {
                            // Leave printable keys unhandled so GPUI routes ASCII & IME
                            // composition through `EntityInputHandler`.
                        }
                    }
                }))
                .child(icon(Icon::Search, 12., text_faint()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(px(12.))
                        .text_color(if is_empty { text_faint() } else { text() })
                        .child(display),
                )
                .child(
                    div()
                        .id("search-clear")
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|s| s.bg(hover()))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.search_input.clear();
                            this.search_open = false;
                            this.rebuild_directory_rows();
                            this.focus.focus(window);
                            cx.notify();
                        }))
                        .child(icon(Icon::Close, 10., text_dim())),
                )
        });

        div()
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(panel())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window);
                    cx.stop_propagation();
                }),
            )
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
                    .when(!self.sidebar_plugins, |d| {
                        d.child(tab("全部", session.entries.len(), DirectoryFilter::All))
                            .child(tab("视频", videos, DirectoryFilter::Videos))
                            .child(tab("照片", photos, DirectoryFilter::Photos))
                            .child(tab("收藏", favorites, DirectoryFilter::Favorites))
                    })
                    .when(self.sidebar_plugins, |d| {
                        d.child(
                            div()
                                .px(px(8.))
                                .text_size(px(12.))
                                .text_color(text())
                                .child("插件面板"),
                        )
                    })
                    .child(div().flex_1())
                    .when(!self.sidebar_plugins, |d| {
                        d.child(search_btn)
                            .child(sort_btn)
                            .when(!compact, |d| d.child(open_btn).children(refresh_btn))
                    })
                    .child(plugins_btn)
                    .child(swap),
            )
            .when(!self.sidebar_plugins, |d| {
                d.children(search_bar).child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h(px(0.))
                        .py(px(4.))
                        .when(row_count > 0, |d| d.child(list).children(scrollbar))
                        .children(note),
                )
            })
            .children(plugin_body)
            .children(resize_handle)
    }

    /// Why the list is empty, and what to do about it.
    fn directory_note(&self) -> impl IntoElement {
        let (title, hint) = if !self.search_input.text().trim().is_empty() {
            ("没有匹配的结果", Some("按 Esc 清空搜索"))
        } else {
            match self.filter {
                DirectoryFilter::Favorites => ("还没有收藏", Some("按 S 收藏正在看的文件")),
                _ if self.active().session.folder.is_none() => {
                    ("未打开文件夹", Some("把文件或文件夹拖进窗口"))
                }
                DirectoryFilter::Videos => ("这个文件夹里没有视频", None),
                DirectoryFilter::Photos => ("这个文件夹里没有照片", None),
                DirectoryFilter::All => ("这个文件夹里没有能打开的文件", None),
            }
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
