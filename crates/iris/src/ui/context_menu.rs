//! Direction C right-click context menu for the viewport and directory rows.
use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    Point, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use super::*;

#[derive(Clone, Debug)]
pub(super) enum ContextTarget {
    Viewport,
    Entry { path: PathBuf, kind: Kind },
}

#[derive(Clone, Debug)]
pub(super) struct ContextMenu {
    pub pos: Point<Pixels>,
    pub target: ContextTarget,
}

impl IrisWindow {
    pub(super) fn render_context_menu(
        &self,
        menu: &ContextMenu,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        const MENU_W: f32 = 216.;
        let vp = window.viewport_size();
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let est_h = match &menu.target {
            ContextTarget::Viewport => 320.,
            ContextTarget::Entry { .. } => 176.,
        };
        let x = f32::from(menu.pos.x).clamp(8., (vw - MENU_W - 8.).max(8.));
        let y = f32::from(menu.pos.y).clamp(8., (vh - est_h - 8.).max(8.));

        let items = match &menu.target {
            ContextTarget::Viewport => self.viewport_context_items(cx),
            ContextTarget::Entry { path, kind } => {
                self.entry_context_items(path.clone(), *kind, cx)
            }
        };

        // Full-window transparent catch layer so clicking anywhere outside closes the menu.
        div()
            .id("context-backdrop")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("context-menu")
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(MENU_W))
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .bg(panel())
                    .border_1()
                    .border_color(line_strong())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items),
            )
    }

    fn viewport_context_items(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let session = &self.active().session;
        let kind = session.kind();
        let paused = session.state.paused;
        let current = session.current.clone();
        let pinned = self.settings.always_on_top;
        let aspect_label = format!("画面比例 · {}", session.state.aspect.label());
        let reveal_label = if cfg!(target_os = "macos") {
            "在访达中显示"
        } else {
            "在资源管理器中显示"
        };

        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if matches!(kind, Some(Kind::Video | Kind::Audio)) {
            out.push(
                menu_row(
                    "ctx-play",
                    if paused { "播放" } else { "暂停" },
                    Some("Space"),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.toggle_pause(&TogglePause, w, cx);
                    }),
                )
                .into_any_element(),
            );
        }
        if kind == Some(Kind::Video) {
            out.push(
                menu_row(
                    "ctx-shot",
                    "截图保存",
                    Some(if cfg!(target_os = "macos") {
                        "⌘S"
                    } else {
                        "Ctrl+S"
                    }),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.take_screenshot(&TakeScreenshot, w, cx);
                    }),
                )
                .into_any_element(),
            );
            out.push(
                menu_row(
                    "ctx-aspect",
                    aspect_label,
                    Some("C"),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.cycle_aspect(&CycleAspect, w, cx);
                    }),
                )
                .into_any_element(),
            );
        }
        if matches!(kind, Some(Kind::Video | Kind::Photo)) {
            out.push(
                menu_row(
                    "ctx-rot",
                    "顺时针旋转 90°",
                    Some("R"),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.rotate_cw(&RotateCw, w, cx);
                    }),
                )
                .into_any_element(),
            );
            out.push(
                menu_row(
                    "ctx-hflip",
                    "水平镜像翻转",
                    Some("H"),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.toggle_hflip(&ToggleHFlip, w, cx);
                    }),
                )
                .into_any_element(),
            );
        }
        if !out.is_empty() {
            out.push(menu_divider().into_any_element());
        }
        out.push(
            menu_row(
                "ctx-pin",
                if pinned {
                    "取消窗口置顶"
                } else {
                    "窗口置顶"
                },
                Some("T"),
                cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx);
                }),
            )
            .into_any_element(),
        );
        if let Some(path) = current {
            out.push(
                menu_row(
                    "ctx-info",
                    "媒体详情与统计",
                    Some("I"),
                    cx.listener(|this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.toggle_info(&ToggleInfo, w, cx);
                    }),
                )
                .into_any_element(),
            );
            let p_reveal = path.clone();
            out.push(
                menu_row(
                    "ctx-reveal",
                    reveal_label,
                    None,
                    cx.listener(move |this, _, _, cx| {
                        this.overlays.context_menu = None;
                        cx.reveal_path(&p_reveal);
                        cx.notify();
                    }),
                )
                .into_any_element(),
            );
            let p_copy = path.to_string_lossy().into_owned();
            out.push(
                menu_row(
                    "ctx-copy",
                    "复制文件路径",
                    None,
                    cx.listener(move |this, _, _, cx| {
                        this.overlays.context_menu = None;
                        cx.write_to_clipboard(ClipboardItem::new_string(p_copy.clone()));
                        this.show_toast("已复制文件路径", cx);
                    }),
                )
                .into_any_element(),
            );
        }
        out.push(menu_divider().into_any_element());
        out.push(
            menu_row(
                "ctx-settings",
                "设置…",
                Some(if cfg!(target_os = "macos") {
                    "⌘,"
                } else {
                    "Ctrl+,"
                }),
                cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.open_settings(&OpenSettings, w, cx);
                }),
            )
            .into_any_element(),
        );
        out
    }

    fn entry_context_items(
        &self,
        path: PathBuf,
        kind: Kind,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let fav = self.store.favorite(&path);
        let has_progress =
            matches!(kind, Kind::Video | Kind::Audio) && self.store.progress_ratio(&path).is_some();
        let reveal_label = if cfg!(target_os = "macos") {
            "在访达中显示"
        } else {
            "在资源管理器中显示"
        };

        let p_open = path.clone();
        let p_fav = path.clone();
        let p_reset = path.clone();
        let p_reveal = path.clone();
        let p_copy = path.to_string_lossy().into_owned();

        let mut out: Vec<gpui::AnyElement> = vec![
            menu_row(
                "ctx-entry-open",
                "打开播放",
                None,
                cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    this.open_path(&p_open, cx);
                }),
            )
            .into_any_element(),
            menu_row(
                "ctx-entry-fav",
                if fav { "取消收藏" } else { "加入收藏" },
                None,
                cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    let added = this.store.toggle_favorite(&p_fav);
                    this.rebuild_directory_rows();
                    this.show_toast(
                        if added {
                            "已收藏"
                        } else {
                            "已取消收藏"
                        },
                        cx,
                    );
                }),
            )
            .into_any_element(),
        ];
        if has_progress {
            out.push(
                menu_row(
                    "ctx-entry-reset",
                    "重置播放进度",
                    None,
                    cx.listener(move |this, _, _, cx| {
                        this.overlays.context_menu = None;
                        if this.store.reset_progress(&p_reset) {
                            this.show_toast("已重置播放进度", cx);
                        }
                    }),
                )
                .into_any_element(),
            );
        }
        out.push(menu_divider().into_any_element());
        out.push(
            menu_row(
                "ctx-entry-reveal",
                reveal_label,
                None,
                cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.reveal_path(&p_reveal);
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        out.push(
            menu_row(
                "ctx-entry-copy",
                "复制文件路径",
                None,
                cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.write_to_clipboard(ClipboardItem::new_string(p_copy.clone()));
                    this.show_toast("已复制文件路径", cx);
                }),
            )
            .into_any_element(),
        );
        out
    }
}

fn menu_row(
    id: &'static str,
    label: impl Into<SharedString>,
    key: Option<&'static str>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(text_muted())
        .hover(|s| s.bg(hover()).text_color(text()))
        .on_click(on_click)
        .child(div().truncate().child(label.into()))
        .when_some(key, |d, key| {
            d.child(
                div()
                    .flex_none()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(text_faint())
                    .child(key),
            )
        })
}

fn menu_divider() -> impl IntoElement {
    div().my(px(4.)).h(px(1.)).w_full().bg(line())
}
