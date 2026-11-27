//! Source browse/resolve, playlist & variant switching, URL resolution, and
//! stream playback cleanup on [`super::PluginController`].

use std::collections::BTreeMap;
use std::time::Instant;

use iris_api::v2::{Capability, ErrorCode, Id};
use iris_core::media_uri::MediaUri;

use super::PluginController;
use super::ports::{
    OpenUrlOutcome, PluginBatchOutcome, PluginContext, PluginPlayer, PluginSideEffect,
    error_code_label,
};
use super::sources::{
    ActiveSource, PendingBrowseRequest, PendingResolveRequest, PluginSourceView, SOURCE_TIMEOUT,
    SourceErrorKind, build_browse_target, find_plugin_playlist_index, remote_search_query,
    save_player_progress_sync, select_url_resolver, validate_resolve_args,
};

impl PluginController {
    pub fn handle_source_error(
        &mut self,
        active_source: &mut ActiveSource,
        plugin_id: &str,
        request_id: Id,
        message: String,
    ) -> (bool, PluginBatchOutcome) {
        let mut out = PluginBatchOutcome::default();
        let Some(kind) = self.browse.take_error(active_source, plugin_id, request_id) else {
            return (false, out);
        };
        match kind {
            SourceErrorKind::Resolve => {
                out.effects.push(PluginSideEffect::MediaLoadCancelled);
                out.domain.playback_changed = true;
            }
            SourceErrorKind::Browse { source_id } => {
                if let ActiveSource::Plugin(ps) = active_source
                    && ps.plugin_id == plugin_id
                    && ps.source_id == source_id
                {
                    ps.error = Some(message.clone());
                    out.domain.catalog_changed = true;
                }
            }
        }
        out.push_toast(format!("数据源错误 · {message}"), false);
        (true, out)
    }

    pub fn on_source_request_timeout(
        &mut self,
        active_source: &mut ActiveSource,
        plugin_id: &str,
        request_id: Id,
    ) -> PluginBatchOutcome {
        let _ = self.window.cancel_request(plugin_id, request_id);
        let (_, out) = self.handle_source_error(
            active_source,
            plugin_id,
            request_id,
            "数据源请求超时".into(),
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub fn browse_source(
        &mut self,
        active_source: &mut ActiveSource,
        plugin_id: &str,
        source_id: &str,
        folder_id: Option<String>,
        page: Option<String>,
        query: Option<String>,
        now: Instant,
    ) -> (bool, PluginBatchOutcome) {
        let mut out = PluginBatchOutcome::default();
        let Some((pid, pname, s)) = self
            .window
            .media_sources()
            .into_iter()
            .find(|(pid, _, s)| *pid == plugin_id && s.id == source_id)
        else {
            out.merge(self.cancel_requests_for(active_source, Some(plugin_id), Some(source_id)));
            out.push_toast("该插件数据源未启用或不可用", false);
            return (false, out);
        };
        let (pid, pname, sname, supports_search, auth_cmd) = (
            pid.to_owned(),
            pname.to_owned(),
            s.name.clone(),
            s.supports_search,
            s.auth_command_id.clone(),
        );
        let same_location = matches!(
            &*active_source,
            ActiveSource::Plugin(ps)
                if ps.plugin_id == pid && ps.source_id == source_id && ps.folder_id == folder_id
        );
        let append = page.is_some() && same_location;
        let (preserved_filters, filters) = if same_location {
            (
                self.browse.source_filters.clone(),
                self.browse.filter_values.clone(),
            )
        } else {
            (Vec::new(), BTreeMap::new())
        };
        let query = query.and_then(|q| remote_search_query(supports_search, &q));
        let issued = if append {
            self.browse
                .view
                .as_ref()
                .filter(|(owner, _)| owner == &pid)
                .ok_or(ErrorCode::StaleHandle)
                .and_then(|(_, view)| {
                    self.window
                        .next_page(&pid, *view, page.as_deref().unwrap_or(""))
                })
        } else {
            out.merge(self.close_browse(active_source));
            self.browse.source_filters = preserved_filters;
            self.browse.filter_values = filters.clone();
            self.window
                .browse(
                    &pid,
                    build_browse_target(source_id, folder_id.clone(), query, filters),
                )
                .map(|(view, request)| {
                    self.browse.view = Some((pid.clone(), view));
                    request
                })
        };
        let req_id = match issued {
            Ok(id) => id,
            Err(error) => {
                log::warn!("browse request rejected for {pid}: {error:?}");
                out.domain.catalog_changed = true;
                out.push_toast(
                    format!("目录请求未能提交 · {}", error_code_label(error)),
                    true,
                );
                return (false, out);
            }
        };
        self.browse.cancel_browse(active_source);
        self.browse.pending_browse = Some(PendingBrowseRequest {
            req_id,
            deadline: now + SOURCE_TIMEOUT,
            plugin_id: pid.clone(),
            source_id: source_id.to_owned(),
            append,
        });
        out.effects.push(PluginSideEffect::ScheduleBrowseTimeout {
            plugin_id: pid.clone(),
            request_id: req_id,
            timeout: SOURCE_TIMEOUT,
        });
        if append && let ActiveSource::Plugin(ps) = active_source {
            ps.loading = true;
            ps.error = None;
        } else {
            *active_source = ActiveSource::Plugin(Box::new(PluginSourceView {
                plugin_id: pid,
                plugin_name: pname,
                source_id: source_id.to_owned(),
                source_name: sname.clone(),
                supports_search,
                auth_command_id: auth_cmd,
                folder_id,
                parent_id: None,
                title: sname,
                next_page: None,
                authenticated: true,
                auth_hint: None,
                loading: true,
                error: None,
            }));
        }
        out.domain.catalog_changed = true;
        (true, out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_entry<P: PluginPlayer>(
        &mut self,
        ctx: PluginContext<'_, P>,
        plugin_id: &str,
        source_id: &str,
        entry_id: &str,
        title: Option<String>,
        variant: Option<String>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        let precheck = if !validate_resolve_args(source_id, entry_id, variant.as_deref()) {
            Err(ErrorCode::InvalidArgument)
        } else {
            self.window
                .precheck_capability(plugin_id, Capability::MediaSource)
        };
        if let Err(error) = precheck {
            log::warn!("resolve precheck rejected for {plugin_id}: {error:?}");
            out.push_toast(
                format!("解析请求未能提交 · {}", error_code_label(error)),
                true,
            );
            return out;
        }
        if self.cancel_resolve_load() {
            out.effects.push(PluginSideEffect::MediaLoadCancelled);
        }
        save_player_progress_sync(
            ctx.player,
            ctx.store,
            ctx.settings.resume_playback,
            self.stream.scoped_released,
        );
        let (advanced, adv_out) = self.advance_media(ctx.player);
        out.merge(adv_out);
        if !advanced {
            return out;
        }
        let folder_id = match &*ctx.active_source {
            ActiveSource::Plugin(ps) if ps.plugin_id == plugin_id && ps.source_id == source_id => {
                ps.folder_id.clone()
            }
            _ => None,
        };
        match self
            .window
            .resolve(plugin_id, source_id, folder_id, entry_id, variant)
        {
            Ok(req_id) => {
                self.browse.pending_resolve = Some(PendingResolveRequest {
                    req_id,
                    deadline: now + SOURCE_TIMEOUT,
                    plugin_id: plugin_id.to_owned(),
                    source_id: source_id.to_owned(),
                    entry_id: entry_id.to_owned(),
                    title,
                });
                out.effects.push(PluginSideEffect::MediaResolveStarted);
                out.effects.push(PluginSideEffect::ScheduleResolveTimeout {
                    plugin_id: plugin_id.to_owned(),
                    request_id: req_id,
                    timeout: SOURCE_TIMEOUT,
                });
                out.domain.playback_changed = true;
            }
            Err(error) => {
                log::warn!("resolve request rejected for {plugin_id}: {error:?}");
                out.push_toast(
                    format!("解析请求未能提交 · {}", error_code_label(error)),
                    true,
                );
            }
        }
        out
    }

    pub fn switch_stream_variant<P: PluginPlayer>(
        &mut self,
        ctx: PluginContext<'_, P>,
        variant_id: &str,
        now: Instant,
    ) -> Option<PluginBatchOutcome> {
        let meta = self.stream.active_stream_meta.clone()?;
        let (pid, sid, eid) = (meta.plugin_id?, meta.source_id?, meta.entry_id?);
        let title = ctx.player.current_title().map(str::to_owned);
        Some(self.resolve_entry(
            ctx,
            &pid,
            &sid,
            &eid,
            title,
            Some(variant_id.to_owned()),
            now,
        ))
    }

    pub fn can_step_playlist(&self, current: Option<&MediaUri>, delta: isize) -> bool {
        let Some(binding) = &self.stream.playlist_binding else {
            return false;
        };
        if !binding.is_live() {
            return false;
        }
        let pid = &binding.installed.plugin_id;
        if self
            .window
            .precheck_capability(pid, Capability::MediaSource)
            .is_err()
        {
            return false;
        }
        let Some(index) = find_plugin_playlist_index(
            &self.stream.playlist,
            pid,
            self.stream.stream_plugin.as_deref(),
            self.stream.playlist_current.as_ref(),
            current,
        ) else {
            return false;
        };
        let len = self.stream.playlist.len() as isize;
        let target = &self.stream.playlist[(index as isize + delta).rem_euclid(len) as usize];
        validate_resolve_args(&target.source_id, &target.entry_id, None)
    }

    pub fn step_playlist<P: PluginPlayer>(
        &mut self,
        ctx: PluginContext<'_, P>,
        current: Option<&MediaUri>,
        delta: isize,
        wrap: bool,
        now: Instant,
    ) -> Option<PluginBatchOutcome> {
        let binding = self.stream.playlist_binding.as_ref()?;
        if !binding.is_live() {
            return None;
        }
        let plugin = binding.installed.plugin_id.clone();
        let index = find_plugin_playlist_index(
            &self.stream.playlist,
            &plugin,
            self.stream.stream_plugin.as_deref(),
            self.stream.playlist_current.as_ref(),
            current,
        )?;
        let next = index as isize + delta;
        let len = self.stream.playlist.len() as isize;
        if !wrap && !(0..len).contains(&next) {
            let mut out = PluginBatchOutcome::default();
            out.push_toast("已播完插件播放列表", false);
            return Some(out);
        }
        let item = self.stream.playlist[next.rem_euclid(len) as usize].clone();
        Some(self.resolve_entry(
            ctx,
            &plugin,
            &item.source_id,
            &item.entry_id,
            Some(item.title),
            None,
            now,
        ))
    }

    pub fn open_user_url<P: PluginPlayer>(
        &mut self,
        ctx: PluginContext<'_, P>,
        url: &str,
        selected: &str,
        now: Instant,
    ) -> (OpenUrlOutcome, PluginBatchOutcome) {
        let mut out = PluginBatchOutcome::default();
        let chosen = match select_url_resolver(&self.window, url, selected) {
            Ok(chosen) => chosen,
            Err(validation) => return (validation, out),
        };
        let Some((plugin, parsed_url)) = chosen else {
            return (
                OpenUrlOutcome::OpenDirect {
                    url: url.to_owned(),
                },
                out,
            );
        };
        if self.cancel_resolve_load() {
            out.effects.push(PluginSideEffect::MediaLoadCancelled);
        }
        save_player_progress_sync(
            ctx.player,
            ctx.store,
            ctx.settings.resume_playback,
            self.stream.scoped_released,
        );
        let (advanced, adv_out) = self.advance_media(ctx.player);
        out.merge(adv_out);
        if !advanced {
            return (
                OpenUrlOutcome::ValidationError("媒体切换暂未完成".into()),
                out,
            );
        }
        match self.window.resolve_url(&plugin, &parsed_url) {
            Ok(req_id) => {
                self.browse.pending_resolve = Some(PendingResolveRequest {
                    req_id,
                    deadline: now + SOURCE_TIMEOUT,
                    plugin_id: plugin.clone(),
                    source_id: String::new(),
                    entry_id: String::new(),
                    title: None,
                });
                out.effects.push(PluginSideEffect::MediaResolveStarted);
                out.effects.push(PluginSideEffect::ScheduleResolveTimeout {
                    plugin_id: plugin,
                    request_id: req_id,
                    timeout: SOURCE_TIMEOUT,
                });
                out.domain.playback_changed = true;
                (OpenUrlOutcome::ResolvedByPlugin, out)
            }
            Err(error) => {
                log::warn!("resolve_url failed for {plugin}: {error:?}");
                (
                    OpenUrlOutcome::ValidationError(format!(
                        "解析请求未能提交 · {}",
                        error_code_label(error)
                    )),
                    out,
                )
            }
        }
    }

    pub fn cleanup_playback<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        pid: Option<&str>,
        sid: Option<&str>,
    ) -> PluginBatchOutcome {
        self.cleanup_playback_ref(&mut ctx, pid, sid)
    }

    pub(super) fn cleanup_playback_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        pid: Option<&str>,
        sid: Option<&str>,
    ) -> PluginBatchOutcome {
        let mut out = self.cancel_requests_for(ctx.active_source, pid, sid);
        if matches!(
            &*ctx.active_source,
            ActiveSource::Plugin(ps)
                if pid.is_none_or(|p| p == ps.plugin_id) && sid.is_none_or(|s| s == ps.source_id)
        ) {
            *ctx.active_source = ActiveSource::Local;
            out.domain.catalog_changed = true;
        }
        let uri_matches = matches!(
            ctx.player.current_media_uri(),
            Some(MediaUri::Plugin {
                plugin_id,
                source_id,
                ..
            }) if pid.is_none_or(|p| p == plugin_id) && sid.is_none_or(|s| s == source_id)
        );
        let meta_matches = self
            .stream
            .active_stream_meta
            .as_ref()
            .is_some_and(|m| m.matches_source(pid, sid));
        let owner_matches = sid.is_none()
            && (self.window.has_active_stream_for(pid)
                || (self.window.has_active_stream_for(None)
                    && pid.is_some_and(|target| {
                        self.stream.stream_plugin.as_deref() == Some(target)
                            || self
                                .stream
                                .playlist_binding
                                .as_ref()
                                .is_some_and(|b| b.installed.plugin_id == target)
                    })));
        let playlist_matches = sid.is_some_and(|target_sid| {
            self.stream
                .playlist_binding
                .as_ref()
                .is_some_and(|b| pid.is_none_or(|p| b.installed.plugin_id == p))
                && self
                    .stream
                    .playlist_current
                    .as_ref()
                    .is_some_and(|(cur_sid, _)| cur_sid == target_sid)
        });
        if uri_matches || meta_matches || owner_matches || playlist_matches {
            save_player_progress_sync(
                ctx.player,
                ctx.store,
                ctx.settings.resume_playback,
                self.stream.scoped_released,
            );
            self.window.clear_active_stream();
            let _ = ctx.player.stop();
            ctx.player.release_scoped();
            self.stream.scoped_released = false;
            self.stream.stream_plugin = None;
            self.stream.playlist_current = None;
            self.stream.active_stream_meta = None;
            out.domain.stream_meta_cleared = true;
            out.domain.playback_changed = true;
            out.effects.push(PluginSideEffect::PlaybackStopped);
        } else if self.stream.clear_meta_for(pid, sid) {
            out.domain.stream_meta_cleared = true;
        }
        if self
            .stream
            .playlist_binding
            .as_ref()
            .is_some_and(|b| pid.is_none_or(|p| b.installed.plugin_id == p))
        {
            if let Some(target_sid) = sid {
                self.stream
                    .playlist
                    .retain(|entry| entry.source_id != target_sid);
                if self
                    .stream
                    .playlist_current
                    .as_ref()
                    .is_some_and(|(cur_sid, _)| cur_sid == target_sid)
                {
                    self.stream.playlist_current = None;
                }
                if self.stream.playlist.is_empty() {
                    self.stream.playlist_binding = None;
                    self.stream.playlist_current = None;
                }
            } else {
                self.stream.clear_playlist();
            }
        }
        out
    }
}
