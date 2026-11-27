use super::{
    super::{AppModel, views::model_listener},
    enhance::Effect,
};
use crate::{
    theme::ThemeTokens,
    ui::kit::{form::chip_button, hstack, vstack},
};
use gpui::{
    App, Context, Entity, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder, px,
};
use iris_api::v2::{
    Capability, ErrorCode, GuestError, HostEvent, Id, Reply, enhance::AnnotationEdit,
};
use iris_core::media_uri::MediaUri;
use iris_core::store::{
    AnnotationEdit as StoreAnnotationEdit, AnnotationError, AnnotationSnapshot, AnnotationTarget,
    ColorLabel, TimeMarker,
};
use iris_net::http_client::HttpCancel;
use iris_plugin_host::{
    ledger::{Ledger, Resource},
    runtime::WindowAccess,
    selection::{
        SelectionGrants, annotation_error_code, edit_to_store, error_code_to_annotation,
        page_to_api,
    },
    ui::Action,
};
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

fn transact_annotations(
    target: &AnnotationTarget,
    edit: Option<(&[MediaUri], &StoreAnnotationEdit)>,
    owner: &str,
    ledger: &Ledger,
    check_live: impl Fn() -> Result<(), AnnotationError>,
) -> Result<AnnotationSnapshot, ErrorCode> {
    target
        .transact(
            edit,
            owner,
            |owner, bytes| {
                ledger
                    .reserve(owner, &[(Resource::HostBytes, bytes)])
                    .map(|lease| Arc::new(lease) as Arc<dyn Any + Send + Sync>)
                    .map_err(error_code_to_annotation)
            },
            check_live,
        )
        .map_err(annotation_error_code)
}

#[derive(Default)]
pub(super) struct AnnotationView {
    pub selections: BTreeMap<String, SelectionGrants>,
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
    pub(super) fn annotation_error(
        &mut self,
        action: &Action,
        code: ErrorCode,
        cx: &mut Context<Self>,
    ) {
        let (iris_api::v2::HostCommand::ReadAnnotation {
            request_id,
            selection,
            ..
        }
        | iris_api::v2::HostCommand::EditAnnotation {
            request_id,
            selection,
            ..
        }) = action.command()
        else {
            return;
        };
        let report = self.plugins.send_to(
            &action.binding.installed.plugin_id,
            HostEvent::AnnotationResult {
                request_id: *request_id,
                selection: *selection,
                result: Reply::Error {
                    error: GuestError {
                        code: code as i32,
                        message: format!("{code:?}"),
                    },
                },
            },
        );
        let regions = self.apply_poll_report(report, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

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
        let Ok(access) = self.plugins.client().access(plugin) else {
            return;
        };
        if !access.capabilities.contains(&Capability::Annotations) {
            return;
        }
        if let Some(binding) = self
            .plugins
            .client()
            .catalog()
            .plugins
            .iter()
            .find(|p| p.id == plugin)
            .and_then(|p| p.binding.clone())
        {
            self.plugin_view
                .enhance
                .bindings
                .insert(access.owner.clone(), binding);
        }
        let grants = self
            .plugin_view
            .enhance
            .annotations
            .selections
            .entry(access.owner.clone())
            .or_insert_with(|| {
                SelectionGrants::new(
                    access.owner.clone(),
                    &access.capabilities,
                    access.resources.ledger.clone(),
                )
            });
        grants.revoke();
        if let Ok(selection) = grants.approve(access.instance, access.generation.media, vec![uri]) {
            let report = self.plugins.send_to(
                plugin,
                HostEvent::SelectionGranted {
                    selection,
                    media: access.generation.media,
                    count: 1,
                },
            );
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn plugin_annotation(
        &mut self,
        action: &Action,
        access: WindowAccess,
        request_id: Id,
        selection: Id,
        edit: Option<AnnotationEdit>,
        page: (u32, u32),
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        access.check(action.update.generation, Capability::Annotations)?;
        let permit = self
            .plugin_view
            .enhance
            .annotations
            .selections
            .get(&access.owner)
            .ok_or(ErrorCode::StaleHandle)?
            .permit(
                &access.owner,
                access.instance,
                access.generation.media,
                selection,
            )?;
        let target = self
            .store
            .annotation_target()
            .map_err(annotation_error_code)?;
        let store_edit = edit.as_ref().map(edit_to_store);
        let binding = action.binding.clone();
        self.enhancement_job(
            action,
            access,
            "annotations".into(),
            move |access| {
                let result: Result<_, ErrorCode> = (|| {
                    let uris = permit.uris()?;
                    let snapshot = transact_annotations(
                        &target,
                        store_edit.as_ref().map(|edit| (uris, edit)),
                        &access.owner,
                        &access.resources.ledger,
                        || {
                            permit.check().map_err(error_code_to_annotation)?;
                            if !binding.is_live() || access.cancel.is_cancelled() {
                                Err(AnnotationError::Cancelled)
                            } else {
                                Ok(())
                            }
                        },
                    )?;
                    let values = if store_edit.is_some() {
                        Vec::new()
                    } else {
                        vec![page_to_api(
                            snapshot
                                .page(uris, page.0, page.1)
                                .map_err(annotation_error_code)?,
                        )]
                    };
                    Ok((snapshot, values))
                })();
                let (snapshot, result) = match result {
                    Ok((snapshot, values)) => (Some(snapshot), Reply::Ok { ok: values }),
                    Err(error) => (
                        None,
                        Reply::Error {
                            error: GuestError {
                                code: error as i32,
                                message: format!("{error:?}"),
                            },
                        },
                    ),
                };
                Ok(Effect::Annotations(
                    snapshot,
                    Box::new(HostEvent::AnnotationResult {
                        request_id,
                        selection,
                        result,
                    }),
                ))
            },
            cx,
        )
    }

    pub(in crate::ui) fn refresh_annotations(&mut self, cx: &mut Context<Self>) {
        self.native_annotation(None, cx);
    }

    fn native_annotation(&mut self, edit: Option<StoreAnnotationEdit>, cx: &mut Context<Self>) {
        let Some(uri) = self.annotation_uri() else {
            return;
        };
        if self.plugin_view.enhance.annotations.busy {
            if edit.is_none() {
                self.plugin_view.enhance.annotations.refresh_pending = true;
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
            let resources = self.plugin_host.resources()?;
            let lease = resources
                .ledger
                .reserve("host.annotation-job", &[(Resource::Task, 1)])?;
            Ok::<_, ErrorCode>((target, resources, lease))
        })();
        let (target, resources, lease) = match setup {
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
        self.plugin_view.enhance.annotations.busy = true;
        let cancel = self.plugin_view.enhance.annotations.cancel.clone();
        let saving = edit.is_some();
        let job = cx.background_executor().spawn(async move {
            let result = transact_annotations(
                &target,
                edit.as_ref().map(|edit| (std::slice::from_ref(&uri), edit)),
                "host.annotation-job",
                &resources.ledger,
                || {
                    if cancel.is_cancelled() {
                        Err(AnnotationError::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            );
            (result, lease)
        });
        cx.spawn(async move |this, cx| {
            let (result, _lease) = job.await;
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.enhance.annotations.busy = false;
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
                if std::mem::take(&mut this.plugin_view.enhance.annotations.refresh_pending) {
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
    let page = m.plugin_view.enhance.annotations.marker_page.min(pages - 1);
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
                                    this.plugin_view.enhance.annotations.marker_page =
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
                                    this.plugin_view.enhance.annotations.marker_page =
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
