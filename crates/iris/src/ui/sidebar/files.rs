//! Unified `Files` sidebar view: 3-variant `DirRow` (`Up`, `Item(CatalogItem)`, `LoadMore`)
//! driving local directories, built-in `iris.webdav`, external `MediaSource` plugins,
//! and favorites with a single virtualized renderer.

use std::ops::Range;
use std::path::PathBuf;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, uniform_list,
};
use iris_core::catalog::{CatalogItem, CatalogItemKind, MediaFilter};
use iris_core::library::Kind;
use iris_core::player::format_time;

use super::super::IrisWindow;
use super::super::context_menu::{ContextMenu, ContextTarget};
use super::super::kit::list_row::{
    MediaRowSpec, folder_row, media_row, row_action_icon, thin_scrollbar,
};
use super::super::kit::text_field::{TextFieldKind, TextFieldSpec};
use super::super::kit::{bordered_button, icon};
use super::super::state::ActiveSource;
use crate::assets::Icon;
use crate::theme::ThemeTokens;

#[derive(Clone, Debug)]
pub(in crate::ui) enum DirRow {
    Up {
        parent_local: Option<PathBuf>,
        parent_plugin: Option<(String, String, Option<String>)>,
    },
    Item(CatalogItem),
    LoadMore {
        plugin_id: String,
        source_id: String,
        folder_id: Option<String>,
        next_page: String,
    },
}

fn item_meta(item: &CatalogItem) -> Option<SharedString> {
    if let Some(dur) = item.duration.filter(|d| *d > 0.) {
        return Some(format_time(dur).into());
    }
    if let Some(sub) = item.subtitle.as_deref().filter(|s| !s.is_empty()) {
        return Some(sub.to_string().into());
    }
    None
}

impl IrisWindow {
    pub(super) fn render_sidebar_files(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let filter = self.catalog.filter;
        let row_count = self.catalog.rows.len();
        let all_count =
            self.catalog.video_count + self.catalog.audio_count + self.catalog.photo_count;

        let source_banner = match &self.catalog.active_source {
            ActiveSource::Local => None,
            ActiveSource::Plugin(ps) => {
                let pid = ps.plugin_id.clone();
                let auth_cmd = ps.auth_command_id.clone();
                let badge = format!("{} · {}", ps.plugin_name, ps.source_name);
                let loading = ps.loading;
                let error = ps.error.clone();
                let authenticated = ps.authenticated;
                let auth_hint = ps.auth_hint.clone();
                Some(
                    div()
                        .flex_none()
                        .px(px(10.))
                        .py(px(6.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .bg(t.window_bg)
                        .border_b_1()
                        .border_color(t.line)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .min_w(px(0.))
                                        .child(icon(Icon::Globe, 12., t.accent))
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(px(11.))
                                                .text_color(t.text)
                                                .child(badge),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("back-to-local")
                                        .px(px(6.))
                                        .py(px(2.))
                                        .rounded(t.radius_sm)
                                        .cursor_pointer()
                                        .text_size(px(10.))
                                        .text_color(t.text_dim)
                                        .hover(|s| s.bg(t.hover).text_color(t.text))
                                        .child("返回本地")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.browse_local_source(cx);
                                        })),
                                ),
                        )
                        .child(self.render_source_filters(cx))
                        .when(loading, |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .child("正在加载远程目录…"),
                            )
                        })
                        .when_some(error, |d, err| {
                            d.child(div().text_size(px(11.)).text_color(t.accent).child(err))
                        })
                        .when(!authenticated, |d| {
                            d.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(11.))
                                            .text_color(t.text_dim)
                                            .child(
                                                auth_hint.unwrap_or_else(|| "需要登录授权".into()),
                                            ),
                                    )
                                    .when_some(auth_cmd, |r, cmd| {
                                        r.child(
                                            bordered_button("src-auth-btn", "去认证", true, t)
                                                .on_click(cx.listener(move |this, _, w, cx| {
                                                    this.invoke_plugin_command(&pid, &cmd, w, cx);
                                                })),
                                        )
                                    }),
                            )
                        }),
                )
            }
        };

        let search_bar = self.text_inputs.search_open.then(|| {
            div()
                .flex_none()
                .p(px(6.))
                .border_b_1()
                .border_color(t.line)
                .child(self.render_text_field(
                    TextFieldSpec {
                        id: "sidebar-search-input".into(),
                        kind: TextFieldKind::Search,
                        placeholder: "筛选当前列表（远程源按 Enter 搜索）…".into(),
                        height: px(28.),
                        leading_icon: Some(Icon::Search),
                        borderless: false,
                    },
                    Some(window),
                    cx,
                ))
        });

        let tabs = [
            ("全部", all_count, MediaFilter::All),
            ("视频", self.catalog.video_count, MediaFilter::Video),
            ("图片", self.catalog.photo_count, MediaFilter::Photo),
            (
                "收藏",
                self.catalog.favorite_count,
                MediaFilter::FavoritesOnly,
            ),
        ];

        let scrollbar = thin_scrollbar(&self.catalog.dir_scroll, t);
        let list = uniform_list(
            "sidebar-files-list",
            row_count,
            cx.processor(|this, range: Range<usize>, _, cx| {
                let t = ThemeTokens::of(cx);
                let cur_uri = this.active().session.current_media_uri();
                range
                    .map(|ix| -> AnyElement {
                        let Some(row) = this.catalog.rows.get(ix).cloned() else {
                            return div().into_any_element();
                        };
                        match row {
                            DirRow::Up {
                                parent_local,
                                parent_plugin,
                            } => folder_row(("dir-up", ix), true, ".. 返回上级", None, t)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(p) = &parent_local {
                                        this.open_path(p, cx);
                                    } else if let Some((pid, sid, fid)) = &parent_plugin {
                                        this.browse_plugin_source(
                                            pid,
                                            sid,
                                            fid.clone(),
                                            None,
                                            None,
                                            cx,
                                        );
                                    }
                                }))
                                .into_any_element(),
                            DirRow::LoadMore {
                                plugin_id,
                                source_id,
                                folder_id,
                                next_page,
                            } => folder_row(("dir-more", ix), false, "加载更多…", None, t)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.browse_plugin_source(
                                        &plugin_id,
                                        &source_id,
                                        folder_id.clone(),
                                        Some(next_page.clone()),
                                        None,
                                        cx,
                                    );
                                }))
                                .into_any_element(),
                            DirRow::Item(item) if item.is_folder() => {
                                let sub = item.subtitle.clone().map(SharedString::from);
                                let title = item.name.clone();
                                let uri_menu = item.uri.clone();
                                let fid_menu = match &item.kind {
                                    CatalogItemKind::Folder { folder_id } => folder_id.clone(),
                                    CatalogItemKind::Media(_) => None,
                                };
                                folder_row(("dir-folder", ix), false, title.clone(), sub, t)
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                            this.overlays.open_context_menu(ContextMenu {
                                                pos: ev.position,
                                                target: ContextTarget::Media {
                                                    uri: uri_menu.clone(),
                                                    title: title.clone(),
                                                    kind: None,
                                                    folder_id: fid_menu.clone(),
                                                },
                                            });
                                            cx.stop_propagation();
                                            cx.notify();
                                        }),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_catalog_item(item.clone(), cx);
                                    }))
                                    .into_any_element()
                            }
                            DirRow::Item(item) => {
                                let thumbnail = this.plugin_thumbnail(&item.uri, cx);
                                let kind = item.media_kind().unwrap_or(Kind::Video);
                                let is_cur = cur_uri.as_ref() == Some(&item.uri);
                                let is_fav = this.store.is_favorite(&item.uri);
                                let prog = this.store.progress_ratio(&item.uri);
                                let uri_star = item.uri.clone();
                                let star_btn = row_action_icon(
                                    ("dir-fav", ix),
                                    if is_fav { Icon::StarFill } else { Icon::Star },
                                    is_fav,
                                    false,
                                    t,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.toggle_favorite(&uri_star);
                                    this.rebuild_directory_rows();
                                    cx.notify();
                                }))
                                .into_any_element();

                                let uri_menu = item.uri.clone();
                                let title_menu = item.name.clone();
                                media_row(
                                    ("dir-media", ix),
                                    MediaRowSpec {
                                        thumbnail,
                                        kind,
                                        title: item.name.clone().into(),
                                        meta: item_meta(&item),
                                        is_current: is_cur,
                                        is_live: item.is_live,
                                        progress: prog,
                                        trailing: Some(star_btn),
                                    },
                                    t,
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                        this.overlays.open_context_menu(ContextMenu {
                                            pos: ev.position,
                                            target: ContextTarget::Media {
                                                uri: uri_menu.clone(),
                                                title: title_menu.clone(),
                                                kind: Some(kind),
                                                folder_id: None,
                                            },
                                        });
                                        cx.stop_propagation();
                                        cx.notify();
                                    }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_catalog_item(item.clone(), cx);
                                }))
                                .into_any_element()
                            }
                        }
                    })
                    .collect()
            }),
        )
        .track_scroll(self.catalog.dir_scroll.clone())
        .size_full();

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .children(source_banner)
            .children(search_bar)
            .child(
                div()
                    .h(px(28.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .border_b_1()
                    .border_color(t.line)
                    .children(tabs.into_iter().enumerate().map(|(i, (lbl, cnt, f))| {
                        let active = f == filter;
                        div()
                            .id(("dir-ftab", i))
                            .h(px(22.))
                            .px(px(7.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .rounded(t.radius_sm)
                            .cursor_pointer()
                            .text_size(px(11.))
                            .when(active, |d| d.bg(t.selected).text_color(t.text))
                            .when(!active, |d| {
                                d.text_color(t.text_dim)
                                    .hover(|s| s.bg(t.hover).text_color(t.text))
                            })
                            .child(format!("{lbl} {cnt}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.catalog.filter = f;
                                this.filter_directory_rows();
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .when(row_count == 0, |d| {
                        d.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.))
                                .text_color(t.text_faint)
                                .child(
                                    if filter == MediaFilter::FavoritesOnly
                                        && self.catalog.favorite_items.is_empty()
                                        && self.catalog.favorite_count > 0
                                    {
                                        "正在加载收藏…"
                                    } else {
                                        "暂无匹配的目录条目"
                                    },
                                ),
                        )
                    })
                    .when(row_count > 0, |d| d.child(list).children(scrollbar)),
            )
    }
}
