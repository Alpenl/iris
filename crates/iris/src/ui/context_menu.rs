//! Unified right-click context menu: `ContextTarget::Viewport` and a single
//! `ContextTarget::Media` covering local files/folders, HTTP streams, and plugin URIs.

use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, StatefulInteractiveElement, Styled, Window, div, px,
};
use iris_core::catalog::QueueItem;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::vfs::PinnedBookmark;

use super::IrisWindow;
use super::actions::{
    CycleAspect, OpenFile, OpenSettings, RotateCw, TakeScreenshot, ToggleAlwaysOnTop, ToggleInfo,
    ToggleLoop, TogglePause,
};
use super::kit::modal::{menu_divider, menu_row};
use super::state::BuiltinDialog;
use crate::assets::Icon;
use crate::platform::REVEAL_IN_FILE_MANAGER_LABEL;
use crate::theme::ThemeTokens;

#[derive(Clone, Debug)]
pub(super) enum ContextTarget {
    Viewport,
    Media {
        uri: MediaUri,
        title: String,
        kind: Option<Kind>,
        folder_id: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ContextMenu {
    pub(super) pos: Point<Pixels>,
    pub(super) target: ContextTarget,
}

impl IrisWindow {
    pub(super) fn render_context_menu(
        &self,
        menu: &ContextMenu,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        const MENU_W: f32 = 236.;
        let vp = window.viewport_size();
        let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
        let items = match &menu.target {
            ContextTarget::Viewport => self.viewport_context_items(t, cx),
            ContextTarget::Media {
                uri,
                title,
                kind,
                folder_id,
            } => self.media_context_items(
                uri.clone(),
                title.clone(),
                *kind,
                folder_id.clone(),
                t,
                cx,
            ),
        };
        let est_h = (items.len() as f32 * f32::from(t.row_h)).clamp(120., 420.);
        let x = f32::from(menu.pos.x).clamp(8., (vw - MENU_W - 8.).max(8.));
        let y = f32::from(menu.pos.y).clamp(8., (vh - est_h - 8.).max(8.));

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
                    .p(px(4.))
                    .flex()
                    .flex_col()
                    .rounded(t.radius_md)
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.line_strong)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items),
            )
    }

    fn viewport_context_items(&self, t: ThemeTokens, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let session = &self.active().session;
        let kind = session.kind();
        let st = session.state();
        let cur_uri = session.current_media_uri();
        let cur_local = session.current().map(std::path::Path::to_path_buf);
        let pinned = self.settings.always_on_top;

        let mut out: Vec<AnyElement> = Vec::new();
        if matches!(kind, Some(Kind::Video | Kind::Audio)) {
            out.push(
                menu_row(
                    "cm-vp-play",
                    Some(if st.paused { Icon::Play } else { Icon::Pause }),
                    if st.paused { "播放" } else { "暂停" },
                    self.key_hint("toggle_pause"),
                    false,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.toggle_pause(&TogglePause, w, cx);
                }))
                .into_any_element(),
            );
            out.push(
                menu_row(
                    "cm-vp-loop",
                    Some(Icon::Repeat),
                    if st.loop_file {
                        "取消单曲循环"
                    } else {
                        "开启单曲循环"
                    },
                    self.key_hint("toggle_loop"),
                    st.loop_file,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.toggle_loop(&ToggleLoop, w, cx);
                }))
                .into_any_element(),
            );
            out.push(menu_divider(t).into_any_element());
        }

        if kind == Some(Kind::Video) {
            out.push(
                menu_row(
                    "cm-vp-aspect",
                    Some(Icon::Aspect),
                    format!("画面比例 · {}", st.aspect.label()),
                    self.key_hint("cycle_aspect"),
                    false,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.cycle_aspect(&CycleAspect, w, cx);
                }))
                .into_any_element(),
            );
        }
        if matches!(kind, Some(Kind::Video | Kind::Photo)) {
            out.push(
                menu_row(
                    "cm-vp-rot",
                    Some(Icon::Rotate),
                    "顺时针旋转 90°",
                    self.key_hint("rotate_cw"),
                    false,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.rotate_cw(&RotateCw, w, cx);
                }))
                .into_any_element(),
            );
            out.push(
                menu_row(
                    "cm-vp-shot",
                    Some(Icon::Camera),
                    "保存当前帧截图",
                    self.key_hint("take_screenshot"),
                    false,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.take_screenshot(&TakeScreenshot, w, cx);
                }))
                .into_any_element(),
            );
        }
        if cur_uri.is_some() {
            out.push(
                menu_row(
                    "cm-vp-info",
                    Some(Icon::Info),
                    "媒体详情与统计",
                    self.key_hint("toggle_info"),
                    self.overlays.info_open,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.toggle_info(&ToggleInfo, w, cx);
                }))
                .into_any_element(),
            );
            out.push(menu_divider(t).into_any_element());
        }

        out.push(
            menu_row(
                "cm-vp-open",
                Some(Icon::FolderOpen),
                "打开本地文件…",
                self.key_hint("open_file"),
                false,
                t,
            )
            .on_click(cx.listener(|this, _, w, cx| {
                this.overlays.context_menu = None;
                this.open_file(&OpenFile, w, cx);
            }))
            .into_any_element(),
        );
        out.push(
            menu_row(
                "cm-vp-url",
                Some(Icon::Globe),
                "打开网络流…",
                self.key_hint("open_url"),
                false,
                t,
            )
            .on_click(cx.listener(|this, _, w, cx| {
                this.overlays.context_menu = None;
                this.open_builtin_dialog(BuiltinDialog::OpenUrl, w, cx);
            }))
            .into_any_element(),
        );
        out.push(
            menu_row(
                "cm-vp-pin",
                Some(Icon::Pin),
                if pinned {
                    "取消窗口置顶"
                } else {
                    "窗口置顶"
                },
                self.key_hint("toggle_always_on_top"),
                pinned,
                t,
            )
            .on_click(cx.listener(|this, _, w, cx| {
                this.overlays.context_menu = None;
                this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx);
            }))
            .into_any_element(),
        );
        if self.effective_hide_titlebar() {
            out.push(
                menu_row(
                    "cm-vp-reset-ui",
                    Some(Icon::Sliders),
                    "恢复默认外观",
                    None,
                    true,
                    t,
                )
                .on_click(cx.listener(|this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.reset_appearance(w, cx);
                }))
                .into_any_element(),
            );
        }
        if let Some(p) = cur_local {
            out.push(menu_divider(t).into_any_element());
            out.push(
                menu_row(
                    "cm-vp-reveal",
                    Some(Icon::Folder),
                    REVEAL_IN_FILE_MANAGER_LABEL,
                    None,
                    false,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.reveal_path(&p);
                    cx.notify();
                }))
                .into_any_element(),
            );
        }
        out.push(menu_divider(t).into_any_element());
        out.push(
            menu_row(
                "cm-vp-settings",
                Some(Icon::Settings),
                "设置…",
                self.key_hint("open_settings"),
                false,
                t,
            )
            .on_click(cx.listener(|this, _, w, cx| {
                this.overlays.context_menu = None;
                this.open_settings(&OpenSettings, w, cx);
            }))
            .into_any_element(),
        );
        out
    }

    fn media_context_items(
        &self,
        uri: MediaUri,
        title: String,
        kind: Option<Kind>,
        folder_id: Option<String>,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        let is_fav = self.store.is_favorite(&uri);
        let in_queue = self.queue.contains(&uri);
        let has_prog = self.store.progress_ratio(&uri).is_some();

        // 1. Open / Browse
        {
            let (u, ttl, fid) = (uri.clone(), title.clone(), folder_id.clone());
            let is_folder = kind.is_none();
            out.push(
                menu_row(
                    "cm-m-open",
                    Some(if is_folder { Icon::Folder } else { Icon::Play }),
                    if is_folder {
                        "进入此目录"
                    } else {
                        "立即打开播放"
                    },
                    None,
                    false,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    if is_folder {
                        match &u {
                            MediaUri::Local(p) => this.open_path(p, cx),
                            MediaUri::Plugin {
                                plugin_id,
                                source_id,
                                entry_id,
                            } => {
                                let f = fid.clone().or_else(|| Some(entry_id.clone()));
                                this.browse_plugin_source(plugin_id, source_id, f, None, None, cx);
                            }
                            MediaUri::HttpUrl(url) => {
                                this.open_direct_stream_url(url, Some(ttl.clone()), cx);
                            }
                        }
                    } else {
                        this.open_media_uri(&u, Some(ttl.clone()), cx);
                    }
                }))
                .into_any_element(),
            );
        }

        // 2. PlayQueue add / remove (for playable media) or Pin Bookmark (for folders)
        if let Some(k) = kind {
            let u_q = uri.clone();
            let ttl_q = title.clone();
            out.push(
                menu_row(
                    "cm-m-queue",
                    Some(if in_queue { Icon::Close } else { Icon::Plus }),
                    if in_queue {
                        "从播放队列移除"
                    } else {
                        "加入播放队列"
                    },
                    None,
                    in_queue,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    if in_queue {
                        this.queue.remove(&u_q);
                        this.show_toast("已从播放队列移除", cx);
                    } else {
                        this.queue
                            .push(QueueItem::new(u_q.clone(), ttl_q.clone(), k));
                        this.show_toast("已加入播放队列", cx);
                    }
                }))
                .into_any_element(),
            );
        } else {
            let u_bm = uri.clone();
            let ttl_bm = title.clone();
            out.push(
                menu_row("cm-m-bm", Some(Icon::Pin), "固定到常驻书签", None, false, t)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.overlays.context_menu = None;
                        let id = format!("bm-{}", this.locations.bookmarks.len() + 1);
                        this.locations.add_bookmark(PinnedBookmark {
                            id,
                            name: ttl_bm.clone(),
                            uri: u_bm.clone(),
                            folder_id: folder_id.clone(),
                        });
                        this.show_toast(format!("已固定书签 · {ttl_bm}"), cx);
                    }))
                    .into_any_element(),
            );
        }

        // 3. Favorite toggle
        {
            let u_fav = uri.clone();
            out.push(
                menu_row(
                    "cm-m-fav",
                    Some(if is_fav { Icon::StarFill } else { Icon::Star }),
                    if is_fav {
                        "取消收藏"
                    } else {
                        "加入收藏"
                    },
                    None,
                    is_fav,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    this.store.toggle_favorite(&u_fav);
                    this.rebuild_directory_rows();
                    cx.notify();
                }))
                .into_any_element(),
            );
        }

        if has_prog {
            let u_rst = uri.clone();
            out.push(
                menu_row(
                    "cm-m-rst",
                    Some(Icon::Refresh),
                    "清除播放进度记录",
                    None,
                    false,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    this.store.reset_progress(&u_rst);
                    cx.notify();
                }))
                .into_any_element(),
            );
        }

        out.push(menu_divider(t).into_any_element());

        if let Some(p) = uri.as_local_path().map(std::path::Path::to_path_buf) {
            out.push(
                menu_row(
                    "cm-m-reveal",
                    Some(Icon::Folder),
                    REVEAL_IN_FILE_MANAGER_LABEL,
                    None,
                    false,
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    cx.reveal_path(&p);
                    cx.notify();
                }))
                .into_any_element(),
            );
        }

        let copy_str = match &uri {
            MediaUri::Local(p) => p.display().to_string(),
            other => other.to_uri_string(),
        };
        out.push(
            menu_row(
                "cm-m-copy",
                Some(Icon::Link),
                "复制路径 / URI",
                None,
                false,
                t,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.overlays.context_menu = None;
                cx.write_to_clipboard(ClipboardItem::new_string(copy_str.clone()));
                this.show_toast("已复制到剪贴板", cx);
            }))
            .into_any_element(),
        );
        out
    }
}
