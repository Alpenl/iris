use super::super::{AppModel, views::model_listener};
use crate::{
    theme::ThemeTokens,
    ui::kit::{form::chip_button, hstack, vstack},
};
use gpui::{
    App, Context, Entity, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder, px,
};
use iris_api::v2::ErrorCode;
use iris_base::net::http_client::HttpCancel;
use iris_core::media_uri::MediaUri;
use iris_core::store::{AnnotationEdit as StoreAnnotationEdit, ColorLabel, TimeMarker};
use iris_plugin_host::selection::annotation_error_code;

#[derive(Default)]
pub(in crate::ui) struct AnnotationView {
    pub cancel: HttpCancel,
    busy: bool,
    refresh_pending: bool,
    marker_page: usize,
}

impl Drop for AnnotationView {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl AppModel {
    pub(super) fn annotation_uri(&self) -> Option<MediaUri> {
        self.active()
            .session
            .may_share_current_identity()
            .then(|| self.active().session.current_media_uri())
            .flatten()
    }

    pub(in crate::ui) fn grant_current_annotations(
        &mut self,
        plugin: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(uri) = self.annotation_uri() else {
            return;
        };
        if let Some(report) = self.plugins.grant_annotations(plugin, vec![uri]) {
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        }
    }

    pub(in crate::ui) fn refresh_annotations(&mut self, cx: &mut Context<Self>) {
        self.native_annotation(None, cx);
    }

    fn native_annotation(&mut self, edit: Option<StoreAnnotationEdit>, cx: &mut Context<Self>) {
        let Some(uri) = self.annotation_uri() else {
            return;
        };
        if self.plugin_view.annotations.busy {
            if edit.is_none() {
                self.plugin_view.annotations.refresh_pending = true;
            } else {
                self.show_toast("标记正在保存，请稍后重试", cx);
            }
            return;
        }
        let setup = (|| {
            let target = self
                .store
                .annotation_target()
                .map_err(annotation_error_code)?;
            let host_job = self.plugin_host.begin_host_annotation()?;
            Ok::<_, ErrorCode>((target, host_job))
        })();
        let (target, host_job) = match setup {
            Ok(value) => value,
            Err(error) => {
                self.show_toast(
                    format!(
                        "个人标记不可用 · {}",
                        super::notices::describe_error_code(error)
                    ),
                    cx,
                );
                return;
            }
        };
        self.plugin_view.annotations.busy = true;
        let cancel = self.plugin_view.annotations.cancel.clone();
        let saving = edit.is_some();
        let job = cx
            .background_executor()
            .spawn(async move { host_job.run(&target, &uri, edit.as_ref(), &cancel) });
        cx.spawn(async move |this, cx| {
            let (result, _lease) = job.await;
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.annotations.busy = false;
                match result {
                    Ok(snapshot) => {
                        let _ = this.store.apply_annotation_snapshot(snapshot);
                        if saving {
                            this.show_toast("个人标记已保存", cx);
                        }
                    }
                    Err(ErrorCode::Cancelled) => {}
                    Err(error) => this.show_toast(
                        format!(
                            "个人标记未更新 · {}",
                            super::notices::describe_error_code(error)
                        ),
                        cx,
                    ),
                }
                if std::mem::take(&mut this.plugin_view.annotations.refresh_pending) {
                    this.refresh_annotations(cx);
                }
                this.notify_viewport_and_dock(cx);
            });
        })
        .detach();
    }
}

pub(in crate::ui) fn render_personal_annotations(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> Option<gpui::AnyElement> {
    let m = model.read(cx);
    let uri = m.annotation_uri()?;
    let empty = iris_core::store::Annotations::default();
    let value = m.store.annotations(&uri).unwrap_or(&empty);
    let current_rating = value.rating;
    let current_color = value.color;
    let marker_count = value.markers.len();
    let ratings = (0u8..=5).map(|rating| {
        chip_button(
            ("rating", rating as usize),
            if rating == 0 {
                "清除".into()
            } else {
                format!("{rating}★")
            },
            current_rating == rating,
            t,
        )
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.native_annotation(Some(StoreAnnotationEdit::Rating { rating }), cx);
        }))
    });
    let colors = [
        (None, "无"),
        (Some(ColorLabel::Red), "红"),
        (Some(ColorLabel::Orange), "橙"),
        (Some(ColorLabel::Yellow), "黄"),
        (Some(ColorLabel::Green), "绿"),
        (Some(ColorLabel::Blue), "蓝"),
        (Some(ColorLabel::Purple), "紫"),
        (Some(ColorLabel::Gray), "灰"),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (color, name))| {
        chip_button(("color-label", i), name, current_color == color, t).on_click(model_listener(
            model,
            move |this, _, _, cx| {
                this.native_annotation(Some(StoreAnnotationEdit::Color { color }), cx);
            },
        ))
    });
    let pages = marker_count.div_ceil(8).max(1);
    let page = m.plugin_view.annotations.marker_page.min(pages - 1);
    let markers: Vec<_> = value
        .markers
        .values()
        .skip(page * 8)
        .take(8)
        .cloned()
        .collect();
    let marker_rows = markers.into_iter().enumerate().map(|(i, marker)| {
        let time = marker.position_ms as f64 / 1000.;
        let id = marker.id;
        hstack(4.)
            .child(
                chip_button(
                    ("marker-seek", i),
                    format!(
                        "{} · {}",
                        iris_core::player::format_time(time),
                        marker.label
                    ),
                    false,
                    t,
                )
                .on_click(model_listener(model, move |this, _, _, cx| {
                    this.command(iris_core::player::Command::SeekAbsolute(time), cx);
                })),
            )
            .child(
                chip_button(("marker-remove", i), "移除", false, t).on_click(model_listener(
                    model,
                    move |this, _, _, cx| {
                        this.native_annotation(
                            Some(StoreAnnotationEdit::RemoveMarker { id: id.clone() }),
                            cx,
                        );
                    },
                )),
            )
    });
    Some(
        vstack(6.)
            .p(px(10.))
            .border_t_1()
            .border_color(t.line)
            .child(
                div()
                    .text_size(t.text_sm)
                    .text_color(t.text_dim)
                    .child(format!("个人标记 · {marker_count} 个时间标记")),
            )
            .child(div().flex().flex_wrap().gap(px(3.)).children(ratings))
            .child(div().flex().flex_wrap().gap(px(3.)).children(colors))
            .child(
                chip_button("add-time-marker", "标记当前时间", false, t).on_click(model_listener(
                    model,
                    |this, _, _, cx| {
                        let position =
                            (this.active().session.state().time_pos.max(0.) * 1000.) as u64;
                        this.native_annotation(
                            Some(StoreAnnotationEdit::UpsertMarker {
                                marker: TimeMarker {
                                    id: format!("mark-{position}"),
                                    position_ms: position,
                                    label: iris_core::player::format_time(position as f64 / 1000.),
                                    color: None,
                                },
                            }),
                            cx,
                        );
                    },
                )),
            )
            .children(marker_rows)
            .when(pages > 1, |d| {
                d.child(
                    hstack(4.)
                        .child(
                            chip_button("marker-prev-page", "上一页", false, t).on_click(
                                model_listener(model, move |this, _, _, cx| {
                                    this.plugin_view.annotations.marker_page =
                                        page.saturating_sub(1);
                                    this.notify_viewport(cx);
                                }),
                            ),
                        )
                        .child(
                            div()
                                .text_size(t.text_sm)
                                .child(format!("{} / {pages}", page + 1)),
                        )
                        .child(
                            chip_button("marker-next-page", "下一页", false, t).on_click(
                                model_listener(model, move |this, _, _, cx| {
                                    this.plugin_view.annotations.marker_page =
                                        (page + 1).min(pages - 1);
                                    this.notify_viewport(cx);
                                }),
                            ),
                        ),
                )
            })
            .into_any_element(),
    )
}
