//! Independent `PlayQueue` sidebar panel: supports mixed local, HTTP(S), and
//! `MediaSource` plugin URIs, appending files or catalog items, item removal, and clear.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    PathPromptOptions, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use iris_core::catalog::QueueItem;
use iris_core::library;
use iris_core::media_uri::MediaUri;

use super::super::IrisWindow;
use super::super::context_menu::{ContextMenu, ContextTarget};
use super::super::kit::bordered_button;
use super::super::kit::list_row::{MediaRowSpec, media_row, row_action_icon};
use crate::assets::Icon;
use crate::theme::ThemeTokens;

impl IrisWindow {
    pub(super) fn prompt_append_queue(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("追加到播放队列".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let items: Vec<QueueItem> = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .map(|p| library::safe_resolve_path(&p))
                        .filter_map(library::entry_from_path)
                        .map(|e| QueueItem::from_local_entry(&e))
                        .collect()
                })
                .await;
            if items.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                let first = items.first().cloned();
                let added = this.queue.extend(items);
                let idle = this.active().session.current_media_uri().is_none();
                if idle && let Some(q) = first {
                    this.open_media_uri(&q.uri, Some(q.title), cx);
                }
                this.show_toast(format!("已追加 {added} 个项目到播放队列"), cx);
            });
        })
        .detach();
    }

    pub(super) fn render_sidebar_queue(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let items: Vec<QueueItem> = self.queue.items().to_vec();
        let cur_uri = self.active().session.current_media_uri();
        let has_catalog = self.catalog.items.iter().any(|i| !i.is_folder());

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        bordered_button("q-add-files", "+ 追加文件…", false, t)
                            .on_click(cx.listener(|this, _, w, cx| this.prompt_append_queue(w, cx))),
                    )
                    .when(has_catalog, |d| {
                        d.child(
                            bordered_button("q-add-cat", "+ 加入当前目录", false, t).on_click(
                                cx.listener(|this, _, _, cx| {
                                    let added = this.queue.extend_catalog(&this.catalog.items);
                                    this.show_toast(format!("已加入 {added} 个目录条目"), cx);
                                }),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .id("sidebar-queue-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p(px(4.))
                    .flex()
                    .flex_col()
                    .when(items.is_empty(), |d| {
                        d.child(
                            div()
                                .p(px(14.))
                                .text_size(px(12.))
                                .text_color(t.text_faint)
                                .child("播放队列为空。在任意本地或远程条目上右键选择「加入播放队列」即可跨目录混合排队。"),
                        )
                    })
                    .children(items.into_iter().enumerate().map(|(ix, item)| {
                        let is_cur = cur_uri.as_ref() == Some(&item.uri);
                        let prog = self.store.progress_ratio(&item.uri);
                        let badge: Option<SharedString> = match &item.uri {
                            MediaUri::Local(_) => None,
                            MediaUri::Plugin { plugin_id, .. } if plugin_id == "iris.webdav" => {
                                Some("DAV".into())
                            }
                            MediaUri::Plugin { .. } => Some("云源".into()),
                            MediaUri::HttpUrl(_) => Some("URL".into()),
                        };
                        let rm_btn = row_action_icon(("q-rm", ix), Icon::Close, false, false, t)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.queue.remove_at(ix);
                                cx.notify();
                            }))
                            .into_any_element();

                        let uri_open = item.uri.clone();
                        let title_open = item.title.clone();
                        let uri_menu = item.uri;
                        let title_menu = item.title.clone();
                        let kind = item.kind;

                        media_row(
                            ("q-row", ix),
                            MediaRowSpec {
                                kind,
                                title: item.title.into(),
                                meta: badge,
                                is_current: is_cur,
                                is_live: false,
                                progress: prog,
                                trailing: Some(rm_btn),
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
                            this.open_media_uri(&uri_open, Some(title_open.clone()), cx);
                        }))
                    })),
            )
    }
}
