use std::time::Instant;

use super::super::{AppModel, state::ActiveSource, views::model_listener};
use crate::theme::ThemeTokens;
use crate::ui::kit::form::chip_button;
use gpui::{
    App, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use iris_app::OpenUrlOutcome;
use iris_core::media_uri::MediaUri;

pub(in crate::ui) use iris_app::remote_search_query;

pub(in crate::ui) fn render_source_filters(model: &Entity<AppModel>, cx: &App) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let filters = m.plugins.source_filters().to_vec();
    div()
        .id("source-filter-options")
        .max_h(px(144.))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(6.))
        .children(filters.into_iter().map(|filter| {
            let id = filter.id;
            let selected = filter.selected;
            let mut options: Vec<(Option<String>, String)> = vec![(None, "不限".into())];
            options.extend(filter.options.into_iter().map(|o| (Some(o.value), o.label)));
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(t.text_xs)
                        .text_color(t.text_dim)
                        .child(filter.label),
                )
                .child(div().flex().flex_wrap().gap(px(4.)).children(
                    options.into_iter().enumerate().map(|(oi, (val, label))| {
                        let active = selected == val;
                        let filter_id = id.clone();
                        chip_button(
                            SharedString::from(format!("source-filter-{filter_id}-{oi}")),
                            label,
                            active,
                            t,
                        )
                        .on_click(model_listener(
                            model,
                            move |this, _, _, cx| {
                                let ActiveSource::Plugin(view) = &this.catalog.active_source else {
                                    return;
                                };
                                let (pid, sid, folder, supports_search) = (
                                    view.plugin_id.clone(),
                                    view.source_id.clone(),
                                    view.folder_id.clone(),
                                    view.supports_search,
                                );
                                this.plugins
                                    .set_source_filter_value(&filter_id, val.clone());
                                let query = remote_search_query(
                                    supports_search && this.text_inputs.search_open,
                                    this.text_inputs.search_input.text(),
                                );
                                this.browse_plugin_source(&pid, &sid, folder, None, query, cx);
                            },
                        ))
                    }),
                ))
        }))
}

pub(in crate::ui) fn render_url_resolver_choice(
    model: &Entity<AppModel>,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let mut choices = vec![
        ("auto".to_owned(), "自动匹配解析插件".to_owned()),
        ("direct".into(), "直接播放媒体地址".into()),
    ];
    choices.extend(m.plugins.url_resolvers(None));
    let selected = m.form_value("url-resolver");
    let active_id = choices
        .iter()
        .find(|(id, _)| id == selected)
        .map(|(id, _)| id.as_str())
        .unwrap_or("auto")
        .to_owned();
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(t.text_sm)
                .text_color(t.text_muted)
                .child("打开方式"),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .children(choices.into_iter().enumerate().map(|(i, (id, label))| {
                    let active = id == active_id;
                    chip_button(("url-resolver-choice", i), label, active, t).on_click(
                        model_listener(model, move |this, _, _, cx| {
                            this.set_form_value("url-resolver", &id);
                            this.overlays.dialog_error = None;
                            this.notify_root(cx);
                        }),
                    )
                })),
        )
}

impl AppModel {
    pub(in crate::ui) fn can_step_plugin_playlist(
        &self,
        current: Option<&MediaUri>,
        delta: isize,
    ) -> bool {
        self.plugins.can_step_playlist(current, delta)
    }

    pub(in crate::ui) fn step_plugin_playlist(
        &mut self,
        current: Option<&MediaUri>,
        delta: isize,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(out) = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.step_playlist(ctx, current, delta, wrap, Instant::now())
        }) else {
            return false;
        };
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
        true
    }

    pub(in crate::ui) fn open_user_url(
        &mut self,
        url: &str,
        selected: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let (outcome, batch) = self
            .with_plugin_ctx(|ctrl, ctx| ctrl.open_user_url(ctx, url, selected, Instant::now()));
        let regions = self.apply_plugin_outcome(batch, None, cx);
        match outcome {
            OpenUrlOutcome::ValidationError(msg) => {
                self.overlays.dialog_error = Some(msg.into());
                self.notify_regions(
                    regions.union(super::super::views::DirtyRegions::root_only()),
                    cx,
                );
                false
            }
            OpenUrlOutcome::OpenDirect { url } => {
                let trimmed = url.trim();
                if trimmed.contains("://")
                    && iris_base::net::http_client::validate_stream_url(trimmed).is_err()
                {
                    self.overlays.dialog_error =
                        Some("仅支持合法的 http:// 或 https:// 媒体地址".into());
                    self.notify_regions(
                        regions.union(super::super::views::DirtyRegions::root_only()),
                        cx,
                    );
                    return false;
                }
                if regions.any() {
                    self.notify_regions(regions, cx);
                }
                self.open_direct_stream_url(&url, None, cx);
                true
            }
            OpenUrlOutcome::ResolvedByPlugin => {
                self.notify_all(cx);
                true
            }
        }
    }
}
