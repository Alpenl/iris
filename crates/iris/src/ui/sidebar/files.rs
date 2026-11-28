//! Unified `Files` sidebar view: 3-variant [`iris_app::DirRow`] (`Up`, `Item(CatalogItem)`, `LoadMore`)
//! driving local directories, `MediaSource` plugins, and favorites without render mutations.

use std::ops::Range;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, uniform_list,
};
use iris_app::{ActiveSource, DirRow};
use iris_core::catalog::{CatalogItem, CatalogItemKind, MediaFilter};
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::player::format_time;

use super::super::context_menu::{ContextMenu, ContextTarget};
use super::super::kit::list_row::{
    MediaRowSpec, folder_row, media_row, row_action_icon, thin_scrollbar,
};
use super::super::kit::text_field::{TextFieldKind, TextFieldSpec, render_text_field};
use super::super::kit::{bordered_button, icon, tab_strip, tab_strip_item};
use super::super::plugin_runtime::sources::render_source_filters;
use super::super::views::{DirtyRegions, Sidebar, model_listener};
use crate::assets::Icon;
use crate::theme::ThemeTokens;

fn item_meta(item: &CatalogItem) -> Option<SharedString> {
    if let Some(dur) = item.duration.filter(|d| *d > 0.) {
        return Some(format_time(dur).into());
    }
    if let Some(sub) = item.subtitle.as_deref().filter(|s| !s.is_empty()) {
        return Some(sub.to_string().into());
    }
    None
}

impl Sidebar {
    pub(super) fn render_sidebar_files(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let model = self.model.clone();
        let m = model.read(cx);
        let t = ThemeTokens::of(cx);
        let filter = m.catalog.filter();
        let row_count = m.catalog.rows().len();
        let counts = m.catalog.counts();
        let all_count = counts.total_media();
        let video_count = counts.video;
        let photo_count = counts.photo;
        let favorite_count = counts.favorite;
        let favorite_items_empty = m.catalog.favorite_items().is_empty();
        let dir_scroll = m.catalog_ui.dir_scroll.clone();
        let search_open = m.text_inputs.search_open;
        let has_source_filters = !m.plugins.source_filters().is_empty();

        let source_banner = match m.catalog.active_source() {
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
                                                .text_size(t.text_sm)
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
                                        .text_size(t.text_xs)
                                        .text_color(t.text_dim)
                                        .hover(|s| s.bg(t.hover).text_color(t.text))
                                        .child("返回本地")
                                        .on_click(model_listener(&model, |this, _, _, cx| {
                                            this.browse_local_source(cx);
                                        })),
                                ),
                        )
                        .when(has_source_filters, |d| {
                            d.child(render_source_filters(&model, cx))
                        })
                        .when(loading, |d| {
                            d.child(
                                div()
                                    .text_size(t.text_sm)
                                    .text_color(t.text_dim)
                                    .child("正在加载远程目录…"),
                            )
                        })
                        .when_some(error, |d, err| {
                            d.child(div().text_size(t.text_sm).text_color(t.danger).child(err))
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
                                            .text_size(t.text_sm)
                                            .text_color(t.text_dim)
                                            .child(
                                                auth_hint.unwrap_or_else(|| "需要登录授权".into()),
                                            ),
                                    )
                                    .when_some(auth_cmd, |r, cmd| {
                                        r.child(
                                            bordered_button("src-auth-btn", "去认证", true, t)
                                                .on_click(model_listener(
                                                    &model,
                                                    move |this, _, w, cx| {
                                                        this.invoke_plugin_command(
                                                            &pid, &cmd, w, cx,
                                                        );
                                                    },
                                                )),
                                        )
                                    }),
                            )
                        }),
                )
            }
        };

        let search_bar = search_open.then(|| {
            div()
                .flex_none()
                .p(px(6.))
                .border_b_1()
                .border_color(t.line)
                .child(render_text_field(
                    &model,
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
            ("视频", video_count, MediaFilter::Video),
            ("图片", photo_count, MediaFilter::Photo),
            ("收藏", favorite_count, MediaFilter::FavoritesOnly),
        ];

        let scrollbar = thin_scrollbar(&dir_scroll, t);
        let list = uniform_list(
            "sidebar-files-list",
            row_count,
            cx.processor(|sidebar, range: Range<usize>, window, cx| {
                let model = sidebar.model.clone();
                let m = model.read(cx);
                let t = ThemeTokens::of(cx);
                let cur_uri = m.session().current_media_uri();
                let mut missing_thumbs: Vec<MediaUri> = Vec::new();

                let elements: Vec<AnyElement> = range
                    .map(|ix| -> AnyElement {
                        let Some(row) = m.catalog.rows().get(ix).cloned() else {
                            return div().into_any_element();
                        };
                        match row {
                            DirRow::Up {
                                parent_local,
                                parent_plugin,
                            } => folder_row(("dir-up", ix), true, ".. 返回上级", None, t)
                                .on_click(model_listener(&model, move |this, _, _, cx| {
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
                                .on_click(model_listener(&model, move |this, _, _, cx| {
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
                                        model_listener(
                                            &model,
                                            move |this, ev: &MouseDownEvent, w, cx| {
                                                this.overlays.open_context_menu(ContextMenu {
                                                    pos: ev.position,
                                                    target: ContextTarget::Media {
                                                        uri: uri_menu.clone(),
                                                        title: title.clone(),
                                                        kind: None,
                                                        folder_id: fid_menu.clone(),
                                                    },
                                                });
                                                this.reconcile_input_focus(w, cx);
                                                cx.stop_propagation();
                                                this.notify_root(cx);
                                            },
                                        ),
                                    )
                                    .on_click(model_listener(&model, move |this, _, _, cx| {
                                        this.open_catalog_item(item.clone(), cx);
                                    }))
                                    .into_any_element()
                            }
                            DirRow::Item(item) => {
                                let thumbnail = m.cached_plugin_thumbnail(&item.uri);
                                if thumbnail.is_none() && m.needs_plugin_thumbnail(&item.uri) {
                                    missing_thumbs.push(item.uri.clone());
                                }
                                let kind = item.media_kind().unwrap_or(Kind::Video);
                                let is_cur = cur_uri.as_ref() == Some(&item.uri);
                                let is_fav = m.catalog.store().is_favorite(&item.uri);
                                let prog = m.catalog.store().progress_ratio(&item.uri);
                                let uri_star = item.uri.clone();
                                let star_btn = row_action_icon(
                                    ("dir-fav", ix),
                                    if is_fav { Icon::StarFill } else { Icon::Star },
                                    is_fav,
                                    false,
                                    t,
                                )
                                .on_click(model_listener(&model, move |this, _, _, cx| {
                                    let cur_uri = this.session().current_media_uri();
                                    let cur_local = this.playback.session().current();
                                    this.catalog.toggle_favorite(
                                        &uri_star,
                                        &this.plugins,
                                        cur_uri.as_ref(),
                                        cur_local,
                                    );
                                    if !this.catalog.is_favorite_scan_in_flight() {
                                        this.catalog_ui.favorite_scan = None;
                                    }
                                    this.scan_visible_favorites(cx);
                                    this.notify_regions(
                                        DirtyRegions {
                                            sidebar: true,
                                            dock: true,
                                            ..DirtyRegions::NONE
                                        },
                                        cx,
                                    );
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
                                    model_listener(
                                        &model,
                                        move |this, ev: &MouseDownEvent, w, cx| {
                                            this.overlays.open_context_menu(ContextMenu {
                                                pos: ev.position,
                                                target: ContextTarget::Media {
                                                    uri: uri_menu.clone(),
                                                    title: title_menu.clone(),
                                                    kind: Some(kind),
                                                    folder_id: None,
                                                },
                                            });
                                            this.reconcile_input_focus(w, cx);
                                            cx.stop_propagation();
                                            this.notify_root(cx);
                                        },
                                    ),
                                )
                                .on_click(model_listener(&model, move |this, _, _, cx| {
                                    this.open_catalog_item(item.clone(), cx);
                                }))
                                .into_any_element()
                            }
                        }
                    })
                    .collect();

                if !missing_thumbs.is_empty() {
                    let weak = sidebar.model.downgrade();
                    window.on_next_frame(move |_, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.request_plugin_thumbnails(missing_thumbs, cx);
                        });
                    });
                }
                elements
            }),
        )
        .track_scroll(dir_scroll)
        .size_full();

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .children(source_banner)
            .children(search_bar)
            .child(
                tab_strip(t).children(tabs.into_iter().enumerate().map(|(i, (lbl, cnt, f))| {
                    let active = f == filter;
                    tab_strip_item(("dir-ftab", i), format!("{lbl} {cnt}"), None, active, t)
                        .on_click(model_listener(&model, move |this, _, _, cx| {
                            let cur_uri = this.session().current_media_uri();
                            this.catalog.set_filter(f, cur_uri.as_ref());
                            this.scan_visible_favorites(cx);
                            this.notify_sidebar(cx);
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
                                .text_size(t.text_md)
                                .text_color(t.text_faint)
                                .child(
                                    if filter == MediaFilter::FavoritesOnly
                                        && favorite_items_empty
                                        && favorite_count > 0
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
