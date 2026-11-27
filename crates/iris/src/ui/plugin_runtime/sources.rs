use super::super::{AppModel, state::ActiveSource, views::model_listener};
use crate::theme::ThemeTokens;
use crate::ui::kit::form::chip_button;
use gpui::{
    App, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use iris_api::v2::{Capability, source::PlaylistEntry};
use iris_core::media_uri::MediaUri;

pub(in crate::ui) fn resolve_playlist_current(
    playlist: &[PlaylistEntry],
    playback_source_id: Option<&str>,
    playback_entry_id: Option<&str>,
    requested: Option<(&str, &str)>,
) -> Option<(String, String)> {
    if playlist.is_empty() {
        return None;
    }
    if let (Some(sid), Some(eid)) = (playback_source_id, playback_entry_id)
        && playlist
            .iter()
            .any(|p| p.source_id == sid && p.entry_id == eid)
    {
        return Some((sid.to_owned(), eid.to_owned()));
    }
    if let Some(eid) = playback_entry_id {
        if let Some((req_sid, _)) = requested
            && playlist
                .iter()
                .any(|p| p.source_id == req_sid && p.entry_id == eid)
        {
            return Some((req_sid.to_owned(), eid.to_owned()));
        }
        if let Some(hit) = playlist.iter().find(|p| p.entry_id == eid) {
            return Some((hit.source_id.clone(), hit.entry_id.clone()));
        }
    }
    if let Some((req_sid, req_eid)) = requested {
        if playlist
            .iter()
            .any(|p| p.source_id == req_sid && p.entry_id == req_eid)
        {
            return Some((req_sid.to_owned(), req_eid.to_owned()));
        }
        if let Some(hit) = playlist.iter().find(|p| p.entry_id == req_eid) {
            return Some((hit.source_id.clone(), hit.entry_id.clone()));
        }
    }
    playlist
        .first()
        .map(|first| (first.source_id.clone(), first.entry_id.clone()))
}

pub(in crate::ui) fn find_plugin_playlist_index(
    playlist: &[PlaylistEntry],
    playlist_plugin_id: &str,
    stream_plugin_id: Option<&str>,
    playlist_current: Option<&(String, String)>,
    current_uri: Option<&MediaUri>,
) -> Option<usize> {
    if playlist.len() <= 1 {
        return None;
    }
    match current_uri {
        Some(MediaUri::Local(_)) => return None,
        Some(MediaUri::Plugin { plugin_id, .. }) if plugin_id != playlist_plugin_id => {
            return None;
        }
        Some(MediaUri::HttpUrl(_)) | None if stream_plugin_id != Some(playlist_plugin_id) => {
            return None;
        }
        _ => {}
    }
    if let Some((sid, eid)) = playlist_current
        && let Some(ix) = playlist
            .iter()
            .position(|p| p.source_id == *sid && p.entry_id == *eid)
    {
        return Some(ix);
    }
    if let Some(MediaUri::Plugin {
        plugin_id,
        source_id,
        entry_id,
    }) = current_uri
        && plugin_id == playlist_plugin_id
    {
        return playlist
            .iter()
            .position(|p| p.source_id == *source_id && p.entry_id == *entry_id);
    }
    None
}

pub(in crate::ui) fn remote_search_query(supports_search: bool, raw: &str) -> Option<String> {
    supports_search
        .then(|| raw.trim())
        .filter(|q| !q.is_empty())
        .map(str::to_owned)
}

pub(in crate::ui) fn render_source_filters(model: &Entity<AppModel>, cx: &App) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let filters = m.plugin_view.source_filters.clone();
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
                                if let Some(f) = this
                                    .plugin_view
                                    .source_filters
                                    .iter_mut()
                                    .find(|f| f.id == filter_id)
                                {
                                    f.selected = val.clone();
                                }
                                if let Some(value) = &val {
                                    this.plugin_view
                                        .filter_values
                                        .insert(filter_id.clone(), value.clone());
                                } else {
                                    this.plugin_view.filter_values.remove(&filter_id);
                                }
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
        let Some(binding) = &self.plugin_view.playlist_binding else {
            return false;
        };
        if !binding.is_live() {
            return false;
        }
        let pid = &binding.installed.plugin_id;
        if self
            .precheck_plugin_capability(pid, Capability::MediaSource)
            .is_err()
        {
            return false;
        }
        let Some(index) = find_plugin_playlist_index(
            &self.plugin_view.playlist,
            pid,
            self.plugin_view.stream_plugin.as_deref(),
            self.plugin_view.playlist_current.as_ref(),
            current,
        ) else {
            return false;
        };
        let len = self.plugin_view.playlist.len() as isize;
        let target = &self.plugin_view.playlist[(index as isize + delta).rem_euclid(len) as usize];
        iris_api::is_valid_id(&target.source_id)
            && !target.entry_id.is_empty()
            && target.entry_id.len() <= 4096
            && !target.entry_id.chars().any(char::is_control)
    }

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
        if !binding.is_live() {
            return false;
        }
        let plugin = binding.installed.plugin_id.clone();
        let Some(index) = find_plugin_playlist_index(
            &self.plugin_view.playlist,
            &plugin,
            self.plugin_view.stream_plugin.as_deref(),
            self.plugin_view.playlist_current.as_ref(),
            current,
        ) else {
            return false;
        };
        let next = index as isize + delta;
        let len = self.plugin_view.playlist.len() as isize;
        if !wrap && !(0..len).contains(&next) {
            self.show_toast("已播完插件播放列表", cx);
            return true;
        }
        let item = self.plugin_view.playlist[next.rem_euclid(len) as usize].clone();
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

    pub(in crate::ui) fn open_user_url(
        &mut self,
        url: &str,
        selected: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let fail = |this: &mut Self, msg: &str, cx: &mut Context<Self>| {
            this.overlays.dialog_error = Some(msg.to_owned().into());
            this.notify_root(cx);
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
            let Ok(parsed) = iris_base::net::http_client::validate_stream_url(url) else {
                fail(self, "请输入合法的 HTTP/HTTPS 地址", cx);
                return false;
            };
            if !parsed.username().is_empty() || parsed.password().is_some() {
                fail(self, "解析地址不能内嵌账号密码，请通过插件登录", cx);
                return false;
            }
            if let Err(error) =
                self.precheck_plugin_capability(&plugin, iris_api::v2::Capability::UrlResolver)
            {
                let msg = super::notices::describe_error_code(error);
                fail(self, &format!("解析请求未能提交 · {msg}"), cx);
                return false;
            }
            self.cancel_media_loads();
            self.save_progress_sync();
            if !self.advance_plugin_media(cx) {
                return false;
            }
            match self.plugins.client().resolve_url(&plugin, parsed.as_str()) {
                Ok(request) => {
                    self.begin_resolve_request(&plugin, "", String::new(), None, request, cx);
                    self.notify_all(cx);
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
            if trimmed.contains("://")
                && iris_base::net::http_client::validate_stream_url(trimmed).is_err()
            {
                fail(self, "仅支持合法的 http:// 或 https:// 媒体地址", cx);
                return false;
            }
            self.open_direct_stream_url(url, None, cx);
        }
        true
    }
}
