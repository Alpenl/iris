//! Independent `PlayQueue` sidebar panel: supports mixed local, HTTP(S), and
//! `MediaSource` plugin URIs, appending files or catalog items, item removal, and clear.

use std::ops::Range;

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    PathPromptOptions, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, uniform_list,
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
                let idle = this.active().session.current_media_uri().is_none()
                    && !this.media_load_pending();
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
        let item_count = self.queue.len();
        let has_catalog = self.catalog.items.iter().any(|item| !item.is_folder());

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
            .child(if item_count == 0 {
                div()
                    .p(px(14.))
                    .text_size(t.text_md)
                    .text_color(t.text_faint)
                    .child("播放队列为空。在任意本地或远程条目上右键选择「加入播放队列」即可跨目录混合排队。")
                    .into_any_element()
            } else {
                uniform_list(
                    "sidebar-queue-scroll",
                    item_count,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let cur_uri = this.active().session.current_media_uri();
                        let t = ThemeTokens::of(cx);
                        range.filter_map(|ix| {
                            let item = this.queue.get(ix)?;
                            let badge: Option<SharedString> = match &item.uri {
                                MediaUri::Local(_) => None,
                                MediaUri::Plugin {
                                    plugin_id,
                                    source_id,
                                    ..
                                } => Some(this.plugin_source_badge(plugin_id, source_id)),
                                MediaUri::HttpUrl(_) => Some("URL".into()),
                            };
                            let remove = row_action_icon(("q-rm", ix), Icon::Close, false, false, t)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.queue.remove_at(ix);
                                    cx.stop_propagation();
                                    cx.notify();
                                }));
                            Some(media_row(
                                ("q-row", ix),
                                MediaRowSpec {
                                    thumbnail: None,
                                    kind: item.kind,
                                    title: item.title.clone().into(),
                                    meta: badge,
                                    is_current: cur_uri.as_ref() == Some(&item.uri),
                                    is_live: false,
                                    progress: this.store.progress_ratio(&item.uri),
                                    trailing: Some(remove.into_any_element()),
                                },
                                t,
                            )
                            .on_mouse_down(MouseButton::Right, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                if let Some(item) = this.queue.get(ix) {
                                    this.overlays.open_context_menu(ContextMenu {
                                        pos: ev.position,
                                        target: ContextTarget::Media {
                                            uri: item.uri.clone(),
                                            title: item.title.clone(),
                                            kind: Some(item.kind),
                                            folder_id: None,
                                        },
                                    });
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(item) = this.queue.get(ix).cloned() {
                                    this.open_media_uri(&item.uri, Some(item.title), cx);
                                }
                            })))
                        }).collect()
                    }),
                )
                .flex_1()
                .min_h(px(0.))
                .into_any_element()
            })
    }
}
