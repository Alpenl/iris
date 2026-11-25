use super::super::{
    IrisWindow,
    state::{ActiveSource, ActiveStreamMeta, StreamReply},
};
use gpui::Context;
use iris_core::{
    media_uri::MediaUri,
    plugin_v2::{hub::Notice, requests::RequestTarget},
};
use std::sync::Arc;

impl IrisWindow {
    pub(in crate::ui) fn apply_plugin_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        match notice {
            Notice::Page {
                binding,
                generation,
                request,
                page,
            } => {
                if !binding.is_live() || generation.context != self.plugins.generation().context {
                    return;
                }
                let pid = &binding.installed.plugin_id;
                let Some(append) =
                    self.catalog
                        .take_page(pid, &request.0.to_string(), &page.page.source_id)
                else {
                    return;
                };
                if self
                    .plugin_view
                    .view
                    .as_ref()
                    .is_none_or(|(p, v)| p != pid || *v != page.view)
                {
                    return;
                }
                if let ActiveSource::Plugin(view) = &mut self.catalog.active_source {
                    if view.plugin_id != *pid || view.source_id != page.page.source_id {
                        return;
                    }
                    let p = &page.page;
                    view.error = None;
                    view.folder_id = p.folder_id.clone();
                    view.parent_id = p.parent_id.clone();
                    view.title = p.title.clone();
                    view.authenticated = p.authenticated;
                    view.auth_hint = p.auth_hint.clone();
                    view.next_page = p.next_page.clone();
                    if !append {
                        view.entries.clear();
                        self.plugin_view.pages.clear();
                        self.plugin_view.source_filters = p.filters.clone();
                        self.plugin_view.filter_values = p
                            .filters
                            .iter()
                            .filter_map(|f| {
                                f.selected
                                    .as_ref()
                                    .map(|value| (f.id.clone(), value.clone()))
                            })
                            .collect();
                    }
                    // The accepted-page reservation remains live and covers decoded UI rows.
                    view.entries
                        .extend(p.entries.iter().map(|e| iris_api::SourceEntry {
                            id: e.id.clone(),
                            name: e.name.clone(),
                            kind: e.kind,
                            size: e.size.map(|v| v.0),
                            modified_secs: e.modified_secs.map(|v| v.0),
                            duration: e.duration,
                            subtitle: e.subtitle.clone(),
                            variants: e.variants.clone(),
                        }));
                    self.store.backfill_plugin_recent_titles(
                        pid,
                        &p.source_id,
                        p.entries.iter().map(|e| (e.id.as_str(), e.name.as_str())),
                    );
                    self.plugin_view.page_binding = Some(binding);
                    self.plugin_view.pages.push(page);
                    self.rebuild_directory_rows();
                }
            }
            Notice::Playback {
                binding,
                generation,
                ticket,
                mut playback,
                playlist,
                ..
            } => {
                if !binding.is_live()
                    || generation != self.plugins.generation()
                    || ticket.cancel.is_cancelled()
                {
                    return;
                }
                let pid = &binding.installed.plugin_id;
                let Some(StreamReply::Requested {
                    source_id,
                    entry_id,
                    title: fallback_title,
                }) = self
                    .catalog
                    .accept_stream(pid, Some(&ticket.id.0.to_string()))
                else {
                    return;
                };
                if (playback.title.trim().is_empty() || playback.title == "插件媒体")
                    && let Some(fallback) = fallback_title.filter(|t| !t.trim().is_empty())
                {
                    playback.title = fallback;
                }
                let (uri, private, variant) = match &ticket.target {
                    RequestTarget::Resolve {
                        source,
                        entry,
                        variant,
                    } if *source == source_id && *entry == entry_id => (
                        MediaUri::Plugin {
                            plugin_id: pid.clone(),
                            source_id,
                            entry_id,
                        },
                        false,
                        variant.clone(),
                    ),
                    RequestTarget::Url { .. } => (
                        MediaUri::HttpUrl("https://plugin.invalid/private".into()),
                        true,
                        None,
                    ),
                    _ => return,
                };
                self.save_progress_sync();
                self.overlays.close_menus();
                let resume = if !private && self.settings.resume_playback && !playback.is_live {
                    self.store.resume(&uri)
                } else {
                    0.
                };
                let stream_variants = playback.variants.clone();
                let resolved_variant = playback.active_variant.clone();
                let settings = self.settings.clone();
                self.active_mut().loading_visible = false;
                match self.active_mut().session.open_scoped(
                    Arc::new(playback),
                    uri.clone(),
                    &settings,
                    resume,
                ) {
                    Ok(()) => {
                        self.active_mut().session.set_private_identity(private);
                        self.plugin_view.playlist = playlist;
                        self.plugin_view.playlist_binding = Some(binding.clone());
                        self.catalog.active_stream_meta = match uri {
                            MediaUri::Plugin {
                                plugin_id,
                                source_id,
                                entry_id,
                            } => {
                                let entry_variants = match &self.catalog.active_source {
                                    ActiveSource::Plugin(view) => view
                                        .entries
                                        .iter()
                                        .find(|e| e.id == entry_id)
                                        .map(|e| e.variants.clone())
                                        .unwrap_or_default(),
                                    _ => Vec::new(),
                                };
                                Some(ActiveStreamMeta {
                                    variants: if entry_variants.is_empty() {
                                        stream_variants
                                    } else {
                                        entry_variants
                                    },
                                    plugin_id: Some(plugin_id),
                                    source_id: Some(source_id),
                                    entry_id: Some(entry_id),
                                    active_variant: variant.or(resolved_variant),
                                })
                            }
                            _ => None,
                        };
                        self.finish_media_open(cx);
                        self.notify_stream_status(pid, Some(ticket.id), cx);
                    }
                    Err(error) => {
                        self.active_mut().pending_subtitles.clear();
                        self.show_toast(format!("无法打开插件媒体 · {error}"), cx);
                    }
                }
                self.active_mut().image.reset();
                cx.notify();
            }
            Notice::Error {
                plugin: Some(plugin),
                request: Some(id),
                code,
                message,
            } => {
                log::warn!(
                    "plugin request error ({plugin}, req={}): {code:?} {message:?}",
                    id.0
                );
                if self.plugin_view.consent_busy == Some(id) {
                    self.plugin_view.consent_busy = None;
                }
                self.handle_source_error(
                    &plugin,
                    &id.0.to_string(),
                    message.unwrap_or_else(|| describe_error_code(code)),
                    cx,
                );
            }
            Notice::Error {
                plugin,
                request,
                code,
                message,
            } => {
                log::warn!(
                    "plugin error ({}, req={request:?}): {code:?} {message:?}",
                    plugin.as_deref().unwrap_or("unknown")
                );
                if let Some(id) = request {
                    if self.plugin_view.consent_busy == Some(id) {
                        self.plugin_view.consent_busy = None;
                    }
                    if code != iris_api::v2::ErrorCode::Backpressure
                        && self.plugin_view.review.as_ref().is_some_and(|r| r.id == id)
                    {
                        self.plugin_view.review = None;
                        self.plugin_view.review_created = None;
                    }
                }
                self.show_toast(
                    format!(
                        "{} · {}",
                        plugin.as_deref().unwrap_or("插件"),
                        message.unwrap_or_else(|| describe_error_code(code))
                    ),
                    cx,
                );
            }
            Notice::Browser {
                binding,
                generation,
                url,
                ..
            } => {
                if binding.is_live() && generation == self.plugins.generation() {
                    cx.open_url(&url);
                }
            }
            Notice::Review(review) => {
                if let Some(old) = self.plugin_view.review.replace(review) {
                    let _ = self.plugins.client().dismiss_review(old.id);
                }
                self.plugin_view.review_created = Some(std::time::Instant::now());
                self.plugin_view.keep_identity = false;
                self.plugin_view.consent_busy = None;
                self.overlays.close_all();
                cx.notify();
            }
            Notice::Origin(origin) => {
                if let Some(old) = self.plugin_view.origin.replace(origin) {
                    let _ = self.plugins.client().confirm_origin(old.id, false, false);
                }
                self.plugin_view.insecure_auth = false;
                self.plugin_view.consent_busy = None;
                self.overlays.close_all();
                cx.notify();
            }
            Notice::OriginClosed(id) => {
                if self.plugin_view.consent_busy == Some(id) {
                    self.plugin_view.consent_busy = None;
                }
                if self.plugin_view.origin.as_ref().is_some_and(|o| o.id == id) {
                    self.plugin_view.origin = None;
                    cx.notify();
                }
            }
            Notice::Installed(record) => {
                self.plugin_view.review = None;
                self.plugin_view.review_created = None;
                self.plugin_view.consent_busy = None;
                self.show_toast(format!("已安装 {}", record.plugin_id), cx);
            }
            Notice::Uninstalled(plugin_id) => {
                self.reconcile_active_source();
                self.sync_theme_tokens(cx);
                self.show_toast(format!("已删除插件 {plugin_id}"), cx);
                cx.notify();
            }
            Notice::Commands(_) => {} // Consumed into Action/slots by PluginManager.
        }
    }
}

pub(super) fn describe_error_code(code: iris_api::v2::ErrorCode) -> String {
    use iris_api::v2::ErrorCode;
    match code {
        ErrorCode::AuthFailed => "鉴权失败或播放源拒绝访问 (AuthFailed)".into(),
        ErrorCode::Denied => "播放地址或权限被策略拒绝 (Denied)".into(),
        ErrorCode::QuotaExceeded => "插件资源配额不足 (QuotaExceeded)".into(),
        ErrorCode::Timeout => "请求超时 (Timeout)".into(),
        ErrorCode::IoError => "网络读取失败 (IoError)".into(),
        ErrorCode::StaleHandle => "播放会话已过期，请重试 (StaleHandle)".into(),
        ErrorCode::Unsupported => "不支持的媒体格式 (Unsupported)".into(),
        ErrorCode::Backpressure => "请求过于频繁，请稍后重试 (Backpressure)".into(),
        other => format!("{other:?}"),
    }
}
