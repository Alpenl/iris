use super::super::{IrisWindow, state::ActiveSource};
use crate::theme::ThemeTokens;
use crate::ui::kit::form::chip_button;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use iris_core::media_uri::MediaUri;

impl IrisWindow {
    pub(in crate::ui) fn step_plugin_playlist(
        &mut self,
        current: Option<&MediaUri>,
        delta: isize,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(binding) = &self.plugin_view.playlist_binding else {
            return false;
        };
        if !binding.is_live() || self.plugin_view.playlist.is_empty() {
            return false;
        }
        let Some(MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        }) = current
        else {
            return false;
        };
        if binding.installed.plugin_id != *plugin_id {
            return false;
        }
        let Some(index) = self
            .plugin_view
            .playlist
            .iter()
            .position(|p| p.source_id == *source_id && p.entry_id == *entry_id)
        else {
            return false;
        };
        let next = index as isize + delta;
        let len = self.plugin_view.playlist.len() as isize;
        if !wrap && !(0..len).contains(&next) {
            self.show_toast("已播完插件播放列表", cx);
            return true;
        }
        let item = self.plugin_view.playlist[next.rem_euclid(len) as usize].clone();
        let plugin = plugin_id.clone();
        self.resolve_plugin_entry(
            &plugin,
            &item.source_id,
            &item.entry_id,
            Some(item.title),
            None,
            cx,
        );
        true
    }
    pub(in crate::ui) fn render_source_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        div()
            .id("source-filter-options")
            .max_h(px(144.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(self.plugin_view.source_filters.iter().map(|filter| {
                let id = filter.id.clone();
                let selected = filter.selected.clone();
                let mut options: Vec<(Option<String>, String)> = vec![(None, "不限".into())];
                options.extend(
                    filter
                        .options
                        .iter()
                        .map(|o| (Some(o.value.clone()), o.label.clone())),
                );
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(t.text_xs)
                            .text_color(t.text_dim)
                            .child(filter.label.clone()),
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
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let ActiveSource::Plugin(view) = &this.catalog.active_source
                                    else {
                                        return;
                                    };
                                    let (pid, sid, folder) = (
                                        view.plugin_id.clone(),
                                        view.source_id.clone(),
                                        view.folder_id.clone(),
                                    );
                                    if let Some(value) = &val {
                                        this.plugin_view
                                            .filter_values
                                            .insert(filter_id.clone(), value.clone());
                                    } else {
                                        this.plugin_view.filter_values.remove(&filter_id);
                                    }
                                    let query =
                                        (!this.text_inputs.search_input.is_empty()).then(|| {
                                            this.text_inputs.search_input.text().trim().to_owned()
                                        });
                                    this.browse_plugin_source(&pid, &sid, folder, None, query, cx);
                                },
                            ))
                        }),
                    ))
            }))
    }
    pub(in crate::ui) fn render_url_resolver_choice(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let mut choices = vec![
            ("auto".to_owned(), "自动匹配解析插件".to_owned()),
            ("direct".into(), "直接播放媒体地址".into()),
        ];
        choices.extend(self.plugins.url_resolvers(None));
        let selected = self.form_value("url-resolver");
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
            .child(div().flex().flex_wrap().gap(px(4.)).children(
                choices.into_iter().enumerate().map(|(i, (id, label))| {
                    let active = id == active_id;
                    chip_button(("url-resolver-choice", i), label, active, t).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.set_form_value("url-resolver", &id);
                            cx.notify();
                        },
                    ))
                }),
            ))
    }
    pub(in crate::ui) fn open_user_url(
        &mut self,
        url: &str,
        selected: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let fail = |this: &mut Self, msg: &str, cx: &mut Context<Self>| {
            this.overlays.dialog_error = Some(msg.to_owned().into());
            cx.notify();
        };
        let matches = self.plugins.url_resolvers(Some(url));
        let plugin = match selected {
            "direct" => None,
            "auto" | "" if matches.len() > 1 => {
                fail(self, "多个插件匹配此地址，请选择打开方式", cx);
                return false;
            }
            "auto" | "" => matches.first().map(|(id, _)| id.clone()),
            id if self
                .plugins
                .url_resolvers(None)
                .iter()
                .any(|(p, _)| p == id) =>
            {
                Some(id.into())
            }
            _ => {
                fail(self, "所选解析插件已不可用", cx);
                return false;
            }
        };
        if let Some(plugin) = plugin {
            let Ok(parsed) = iris_core::vfs::validate_stream_url(url) else {
                fail(self, "请输入合法的 HTTP/HTTPS 地址", cx);
                return false;
            };
            if !parsed.username().is_empty() || parsed.password().is_some() {
                fail(self, "解析地址不能内嵌账号密码，请通过插件登录", cx);
                return false;
            }
            self.cancel_media_loads();
            if !self.advance_plugin_media(cx) {
                return false;
            }
            match self.plugins.client().resolve_url(&plugin, parsed.as_str()) {
                Ok(request) => {
                    self.begin_resolve_request(&plugin, "", String::new(), None, request, cx)
                }
                Err(error) => {
                    log::warn!("resolve_url failed for {plugin}: {error:?}");
                    let msg = super::notices::describe_error_code(error);
                    fail(self, &format!("解析请求未能提交 · {msg}"), cx);
                    return false;
                }
            }
        } else {
            let trimmed = url.trim();
            if trimmed.contains("://") && iris_core::vfs::validate_stream_url(trimmed).is_err() {
                fail(self, "仅支持合法的 http:// 或 https:// 媒体地址", cx);
                return false;
            }
            self.open_direct_stream_url(url, None, cx);
        }
        true
    }
}
