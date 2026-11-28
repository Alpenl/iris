//! Unified right-click context menu: `ContextTarget::Viewport` and a single
//! `ContextTarget::Media` covering local files/folders, HTTP streams, and plugin URIs.

use gpui::{
    AnyElement, App, ClipboardItem, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, StatefulInteractiveElement, Styled, Window, div, px,
};
use iris_core::catalog::QueueItem;
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;

use super::AppModel;
use super::actions::{ClearAbLoop, CycleAbLoop};
use super::actions::{
    CycleAspect, OpenFile, OpenSettings, RotateCw, TakeScreenshot, ToggleAlwaysOnTop, ToggleInfo,
    ToggleLoop, TogglePause,
};
use super::kit::modal::{menu_divider, menu_row};
use super::state::BuiltinDialog;
use super::views::{DirtyRegions, model_listener};
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

pub(super) fn render_context_menu(
    model: &Entity<AppModel>,
    menu: &ContextMenu,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    const MENU_W: f32 = 236.;
    let vp = window.viewport_size();
    let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
    let mut items = match &menu.target {
        ContextTarget::Viewport => viewport_context_items(model, m, t),
        ContextTarget::Media {
            uri,
            title,
            kind,
            folder_id,
        } => media_context_items(
            model,
            m,
            uri.clone(),
            title.clone(),
            *kind,
            folder_id.clone(),
            t,
        ),
    };
    // Only the current viewport is a supported plugin context. Browsed
    // files are never passed to a command as an implicit selection grant.
    if matches!(menu.target, ContextTarget::Viewport) {
        let commands = m.plugins.commands();
        let mut contributions = commands
            .into_iter()
            .filter(|(_, _, c)| c.context_menu)
            .take(32)
            .peekable();
        if contributions.peek().is_some() {
            items.push(menu_divider(t).into_any_element());
        }
        for (plugin, name, command) in contributions {
            let pid = plugin.to_owned();
            let cid = command.id.clone();
            let label = format!("{name} · {}", command.title);
            items.push(
                menu_row(
                    gpui::SharedString::from(format!("cm-plugin-{pid}-{cid}")),
                    None,
                    label,
                    None,
                    false,
                    t,
                )
                .on_click(model_listener(model, move |this, _, window, cx| {
                    this.overlays.context_menu = None;
                    this.notify_root(cx);
                    this.invoke_plugin_command(&pid, &cid, window, cx);
                }))
                .into_any_element(),
            );
        }
    }
    let menu_w = MENU_W.min((vw - 16.).max(140.));
    let max_h = (vh - 16.).clamp(96., 420.);
    let est_h = (items.len() as f32 * f32::from(t.row_h) + 8.).clamp(96., max_h);
    let x = f32::from(menu.pos.x).clamp(8., (vw - menu_w - 8.).max(8.));
    let y = f32::from(menu.pos.y).clamp(8., (vh - est_h - 8.).max(8.));

    div()
        .id("context-backdrop")
        .absolute()
        .inset_0()
        .on_mouse_down(
            MouseButton::Left,
            model_listener(model, |this, _, _, cx| {
                this.overlays.context_menu = None;
                cx.stop_propagation();
                this.notify_root(cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            model_listener(model, |this, _, _, cx| {
                this.overlays.context_menu = None;
                cx.stop_propagation();
                this.notify_root(cx);
            }),
        )
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(
            div()
                .id("context-menu")
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(menu_w))
                .max_h(px(max_h))
                .overflow_y_scroll()
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

fn viewport_context_items(
    model: &Entity<AppModel>,
    m: &AppModel,
    t: ThemeTokens,
) -> Vec<AnyElement> {
    let session = m.session();
    let kind = session.kind();
    let st = session.state();
    let cur_uri = session.current_media_uri();
    let cur_local = session.current().map(std::path::Path::to_path_buf);
    let pinned = m.settings.always_on_top;

    let mut out: Vec<AnyElement> = Vec::new();
    if matches!(kind, Some(Kind::Video | Kind::Audio)) {
        out.push(
            menu_row(
                "cm-vp-play",
                Some(if st.paused { Icon::Play } else { Icon::Pause }),
                if st.paused { "播放" } else { "暂停" },
                m.key_hint("toggle_pause"),
                false,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
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
                m.key_hint("toggle_loop"),
                st.loop_file,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
                this.toggle_loop(&ToggleLoop, w, cx);
            }))
            .into_any_element(),
        );
        if !st.is_live {
            out.push(
                menu_row(
                    "cm-vp-ab",
                    Some(Icon::Repeat),
                    super::strings::actions::action_title("cycle_ab_loop"),
                    m.key_hint("cycle_ab_loop"),
                    st.ab_start.is_some(),
                    t,
                )
                .on_click(model_listener(model, |this, _, w, cx| {
                    this.overlays.context_menu = None;
                    this.notify_root(cx);
                    this.cycle_ab_loop(&CycleAbLoop, w, cx);
                }))
                .into_any_element(),
            );
            if st.ab_start.is_some() || st.ab_end.is_some() {
                out.push(
                    menu_row(
                        "cm-vp-ab-clear",
                        None,
                        super::strings::actions::action_title("clear_ab_loop"),
                        m.key_hint("clear_ab_loop"),
                        false,
                        t,
                    )
                    .on_click(model_listener(model, |this, _, w, cx| {
                        this.overlays.context_menu = None;
                        this.notify_root(cx);
                        this.clear_ab_loop(&ClearAbLoop, w, cx);
                    }))
                    .into_any_element(),
                );
            }
        }
        out.push(menu_divider(t).into_any_element());
    }

    if kind == Some(Kind::Video) {
        out.push(
            menu_row(
                "cm-vp-aspect",
                Some(Icon::Aspect),
                format!(
                    "画面比例 · {}",
                    super::strings::chrome::aspect_label(st.aspect)
                ),
                m.key_hint("cycle_aspect"),
                false,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
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
                super::strings::actions::action_title("rotate_cw"),
                m.key_hint("rotate_cw"),
                false,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
                this.rotate_cw(&RotateCw, w, cx);
            }))
            .into_any_element(),
        );
        out.push(
            menu_row(
                "cm-vp-shot",
                Some(Icon::Camera),
                super::strings::actions::action_title("take_screenshot"),
                m.key_hint("take_screenshot"),
                false,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
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
                super::strings::chrome::MEDIA_INFO_LABEL,
                m.key_hint("toggle_info"),
                m.overlays.info_open,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
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
            super::strings::actions::action_title("open_file"),
            m.key_hint("open_file"),
            false,
            t,
        )
        .on_click(model_listener(model, |this, _, w, cx| {
            this.overlays.context_menu = None;
            this.notify_root(cx);
            this.open_file(&OpenFile, w, cx);
        }))
        .into_any_element(),
    );
    out.push(
        menu_row(
            "cm-vp-url",
            Some(Icon::Globe),
            super::strings::chrome::OPEN_STREAM_LABEL,
            m.key_hint("open_url"),
            false,
            t,
        )
        .on_click(model_listener(model, |this, _, w, cx| {
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
            m.key_hint("toggle_always_on_top"),
            pinned,
            t,
        )
        .on_click(model_listener(model, |this, _, w, cx| {
            this.overlays.context_menu = None;
            this.toggle_always_on_top(&ToggleAlwaysOnTop, w, cx);
        }))
        .into_any_element(),
    );
    if m.effective_hide_titlebar() {
        out.push(
            menu_row(
                "cm-vp-reset-ui",
                Some(Icon::Sliders),
                super::strings::chrome::RESET_APPEARANCE_LABEL,
                None,
                true,
                t,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                cx.reveal_path(&p);
                this.notify_root(cx);
            }))
            .into_any_element(),
        );
    }
    out.push(menu_divider(t).into_any_element());
    out.push(
        menu_row(
            "cm-vp-settings",
            Some(Icon::Settings),
            super::strings::actions::action_title("open_settings"),
            m.key_hint("open_settings"),
            false,
            t,
        )
        .on_click(model_listener(model, |this, _, w, cx| {
            this.overlays.context_menu = None;
            this.open_settings(&OpenSettings, w, cx);
        }))
        .into_any_element(),
    );
    out
}

fn media_context_items(
    model: &Entity<AppModel>,
    m: &AppModel,
    uri: MediaUri,
    title: String,
    kind: Option<Kind>,
    folder_id: Option<String>,
    t: ThemeTokens,
) -> Vec<AnyElement> {
    let mut out: Vec<AnyElement> = Vec::new();
    let is_fav = m.catalog.store().is_favorite(&uri);
    let in_queue = m.catalog.queue().contains(&uri);
    let has_prog = m.catalog.store().progress_ratio(&uri).is_some();

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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                this.notify_root(cx);
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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                if in_queue {
                    this.catalog.remove_queue_uri(&u_q);
                    this.show_toast("已从播放队列移除", cx);
                } else {
                    this.catalog
                        .push_queue_item(QueueItem::new(u_q.clone(), ttl_q.clone(), k));
                    this.show_toast("已加入播放队列", cx);
                }
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        sidebar: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }))
            .into_any_element(),
        );
    } else {
        let u_bm = uri.clone();
        let ttl_bm = title.clone();
        out.push(
            menu_row("cm-m-bm", Some(Icon::Pin), "固定到常驻书签", None, false, t)
                .on_click(model_listener(model, move |this, _, _, cx| {
                    this.overlays.context_menu = None;
                    this.pin_bookmark(ttl_bm.clone(), u_bm.clone(), folder_id.clone(), cx);
                    this.notify_regions(
                        DirtyRegions {
                            root: true,
                            sidebar: true,
                            ..DirtyRegions::NONE
                        },
                        cx,
                    );
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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                let cur_uri = this.session().current_media_uri();
                let cur_local = this.playback.session().current();
                this.catalog
                    .toggle_favorite(&u_fav, &this.plugins, cur_uri.as_ref(), cur_local);
                if !this.catalog.is_favorite_scan_in_flight() {
                    this.catalog_ui.favorite_scan = None;
                }
                this.scan_visible_favorites(cx);
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        sidebar: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                this.catalog.reset_progress(&u_rst);
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        sidebar: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
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
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.overlays.context_menu = None;
                cx.reveal_path(&p);
                this.notify_root(cx);
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
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.overlays.context_menu = None;
            cx.write_to_clipboard(ClipboardItem::new_string(copy_str.clone()));
            this.show_toast("已复制到剪贴板", cx);
        }))
        .into_any_element(),
    );
    out
}
