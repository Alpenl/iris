use super::super::{
    AppModel,
    state::{ActiveSource, ActiveStreamMeta, StreamReply},
    views::DirtyRegions,
};
use gpui::Context;
use iris_core::media_uri::MediaUri;
use iris_plugin_host::{hub::Notice, requests::RequestTarget};
use std::sync::Arc;

impl AppModel {
    pub(in crate::ui) fn apply_plugin_notice(
        &mut self,
        notice: Notice,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        match notice {
            Notice::Page {
                binding,
                generation,
                request,
                page,
            } => {
                if !binding.is_live() || generation.context != self.plugins.generation().context {
                    return DirtyRegions::NONE;
                }
                let pid = &binding.installed.plugin_id;
                let Some(append) =
                    self.catalog
                        .take_page(pid, &request.0.to_string(), &page.page.source_id)
                else {
                    return DirtyRegions::NONE;
                };
                if self
                    .plugin_view
                    .view
                    .as_ref()
                    .is_none_or(|(p, v)| p != pid || *v != page.view)
                {
                    return DirtyRegions::NONE;
                }
                if let ActiveSource::Plugin(view) = &mut self.catalog.active_source {
                    if view.plugin_id != *pid || view.source_id != page.page.source_id {
                        return DirtyRegions::NONE;
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
                    // The accepted-page reservation remains live and backs decoded UI rows directly.
                    self.store.backfill_plugin_recent_titles(
                        pid,
                        &p.source_id,
                        p.entries.iter().map(|e| (e.id.as_str(), e.name.as_str())),
                    );
                    self.plugin_view.page_binding = Some(binding);
                    self.plugin_view.pages.push(page);
                    self.rebuild_directory_rows();
                    return DirtyRegions {
                        sidebar: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    };
                }
                DirtyRegions::NONE
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
                    return DirtyRegions::NONE;
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
                    return DirtyRegions::NONE;
                };
                if playback.title.trim().is_empty()
                    || playback.title == super::super::strings::chrome::DEFAULT_PLUGIN_MEDIA_TITLE
                {
                    playback.title = fallback_title
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| {
                            super::super::strings::chrome::DEFAULT_PLUGIN_MEDIA_TITLE.into()
                        });
                }
                let requested_target = match &ticket.target {
                    RequestTarget::Resolve { source, entry, .. } => {
                        Some((source.as_str(), entry.as_str()))
                    }
                    _ => None,
                };
                let playlist_current = super::sources::resolve_playlist_current(
                    &playlist,
                    playback.source_id.as_deref(),
                    playback.entry_id.as_deref(),
                    requested_target,
                );
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
                    _ => return DirtyRegions::NONE,
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
                self.active_mut().commit_media_open_generation();
                match self.active_mut().session.open_scoped(
                    Arc::new(playback).into_scoped(),
                    uri.clone(),
                    &settings,
                    resume,
                ) {
                    Ok(()) => {
                        self.active_mut().session.set_private_identity(private);
                        self.plugin_view.stream_plugin = Some(pid.clone());
                        self.plugin_view.playlist_current = playlist_current;
                        self.plugin_view.playlist = playlist;
                        self.plugin_view.playlist_binding = Some(binding.clone());
                        self.catalog.active_stream_meta = match uri {
                            MediaUri::Plugin {
                                plugin_id,
                                source_id,
                                entry_id,
                            } => {
                                let entry_variants = match &self.catalog.active_source {
                                    ActiveSource::Plugin(view)
                                        if view.plugin_id == plugin_id
                                            && view.source_id == source_id =>
                                    {
                                        self.plugin_source_entries(&source_id)
                                            .find(|e| e.id == entry_id)
                                            .map(|e| e.variants.clone())
                                            .unwrap_or_default()
                                    }
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
                        log::warn!("open_scoped failed for {pid}: {error:#}");
                        self.active_mut().pending_subtitles.clear();
                        self.show_error_toast("无法打开插件媒体，请检查网络或鉴权状态", cx);
                    }
                }
                self.active_mut().image.reset();
                self.sync_viewport_effects(cx);
                DirtyRegions::ALL
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
                let mut dirty = DirtyRegions::NONE;
                if let Some(id) = request {
                    self.plugin_view.consent_busy.remove(&id);
                    dirty.root = true;
                } else if let Some(pid) = plugin.as_deref()
                    && self.plugin_view.modal.clear_submitting_for(pid)
                {
                    dirty.root = true;
                }
                let detail = message.unwrap_or_else(|| describe_error_code(code));
                let matched_source = if let (Some(pid), Some(id)) = (plugin.as_deref(), request) {
                    self.handle_source_error(pid, &id.0.to_string(), detail.clone(), cx)
                } else {
                    false
                };
                if !matched_source {
                    if let Some(id) = request
                        && code != iris_api::v2::ErrorCode::Backpressure
                        && self.plugin_view.review.as_ref().is_some_and(|r| r.id == id)
                    {
                        self.plugin_view.review = None;
                        self.plugin_view.review_created = None;
                        self.restore_modal_focus_if_unblocked();
                        dirty.root = true;
                    }
                    if should_fallback_error_toast(matched_source, code) {
                        self.show_toast(
                            format!("{} · {detail}", plugin.as_deref().unwrap_or("插件")),
                            cx,
                        );
                        dirty.root = true;
                    }
                }
                dirty
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
                DirtyRegions::NONE
            }
            Notice::Review(review) => {
                let new_id = review.id;
                if let Some(old) = self.plugin_view.review.replace(review) {
                    let _ = self.plugins.client().dismiss_review(old.id);
                    self.plugin_view.consent_busy.remove(&old.id);
                }
                self.plugin_view.consent_busy.remove(&new_id);
                self.plugin_view.review_created = Some(std::time::Instant::now());
                self.plugin_view.keep_identity = false;
                self.overlays.close_all();
                DirtyRegions::ALL
            }
            Notice::Origin(origin) => {
                let new_id = origin.id;
                if let Some(old) = self.plugin_view.origin.replace(origin) {
                    let _ = self.plugins.client().confirm_origin(old.id, false, false);
                    self.plugin_view.consent_busy.remove(&old.id);
                }
                self.plugin_view.consent_busy.remove(&new_id);
                self.plugin_view.insecure_auth = false;
                self.overlays.close_all();
                DirtyRegions::ALL
            }
            Notice::OriginClosed(id) => {
                self.plugin_view.consent_busy.remove(&id);
                if self.plugin_view.origin.as_ref().is_some_and(|o| o.id == id) {
                    self.plugin_view.origin = None;
                    self.restore_modal_focus_if_unblocked();
                    return DirtyRegions::root_only();
                }
                DirtyRegions::NONE
            }
            Notice::Installed(record) => {
                if let Some(old) = self.plugin_view.review.take() {
                    self.plugin_view.consent_busy.remove(&old.id);
                }
                self.plugin_view.review_created = None;
                self.restore_modal_focus_if_unblocked();
                self.show_toast(format!("已安装 {}", record.plugin_id), cx);
                DirtyRegions::ALL
            }
            Notice::Uninstalled(plugin_id) => {
                self.cleanup_plugin_playback(Some(&plugin_id), None);
                self.reconcile_active_source();
                self.sync_theme_tokens(cx);
                self.sync_viewport_effects(cx);
                self.show_toast(format!("已删除插件 {plugin_id}"), cx);
                DirtyRegions::ALL
            }
            Notice::Commands(_) => DirtyRegions::NONE, // Consumed into Action/slots by PluginManager.
        }
    }
}

fn should_fallback_error_toast(matched_source: bool, code: iris_api::v2::ErrorCode) -> bool {
    !matched_source && code != iris_api::v2::ErrorCode::Cancelled
}

pub(in crate::ui) fn describe_error_code(code: iris_api::v2::ErrorCode) -> String {
    super::super::strings::errors::plugin_error_code(code)
}

#[cfg(test)]
mod tests {
    use super::should_fallback_error_toast;
    use crate::ui::state::{CatalogState, PendingResolve};
    use iris_api::v2::ErrorCode;
    use std::time::{Duration, Instant};

    #[test]
    fn unmatched_plugin_request_error_falls_back_to_toast_unless_cancelled() {
        let mut catalog = CatalogState {
            pending_resolve: Some(PendingResolve {
                req_id: "10".into(),
                deadline: Instant::now() + Duration::from_secs(30),
                plugin_id: "demo".into(),
                source_id: "src".into(),
                entry_id: "item".into(),
                title: None,
                _timeout: None,
            }),
            ..Default::default()
        };
        // Non-catalog request (e.g. Origin approval failure id=99) does not match CatalogState.
        let matched = catalog.take_error("demo", "99").is_some();
        assert!(!matched);
        assert!(should_fallback_error_toast(matched, ErrorCode::Denied));
        assert!(!should_fallback_error_toast(matched, ErrorCode::Cancelled));

        // Catalog resolve request id=10 matches and handles its own toast.
        let matched_resolve = catalog.take_error("demo", "10").is_some();
        assert!(matched_resolve);
        assert!(!should_fallback_error_toast(
            matched_resolve,
            ErrorCode::IoError
        ));
    }
}
