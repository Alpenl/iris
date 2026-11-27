//! Bounded `WindowSession::poll` draining, action/effect execution, and notice
//! routing for [`super::PluginController`].

use std::time::Instant;

use iris_api::v2::{ErrorCode, HostEvent};
use iris_core::media_uri::MediaUri;
use iris_plugin_host::{
    ActionKind, Event as PluginEvent, Notice, PollReport, RequestTarget, WindowAction,
};

use super::enhance::{EnhancementStep, apply_ready_effect_step, execute_enhancement_step};
use super::ports::{
    DEFAULT_PLUGIN_MEDIA_TITLE, PluginBatchOutcome, PluginContext, PluginPlayer, PluginSideEffect,
    error_code_label,
};
use super::sources::{
    ActiveSource, ActiveStreamMeta, StreamReply, resolve_playlist_current,
    save_player_progress_sync, should_fallback_error_toast,
};
use super::{PluginController, TICK_MIN_INTERVAL};

impl PluginController {
    /// Drains bounded `WindowSession` events, reconciles lifetimes, applies actions/effects/notices
    /// in deterministic order, and dispatches ready deferred commands.
    pub fn poll<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let report = self.window.poll();
        self.bind_current_file(ctx.player);
        let mut out = self.reconcile_lifetimes_ref(&mut ctx, now);
        out.merge(self.apply_poll_report_ref(&mut ctx, report, now));
        out.merge(self.run_deferred_commands_ref(&mut ctx, now));
        out
    }

    pub fn broadcast<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        event: PluginEvent,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.bind_current_file(ctx.player);
        let report = self.window.broadcast(&event);
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    pub fn announce_media_if_loaded<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        if !ctx.player.state().loaded {
            return PluginBatchOutcome::default();
        }
        self.bind_current_file(ctx.player);
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let Some(report) = self.window.announce_media_opened(snap) else {
            return PluginBatchOutcome::default();
        };
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    /// Emits `HostEvent::Tick` at most at 5Hz (`200ms`) when state-listening plugins are active.
    pub fn maybe_tick<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        if !self.window.has_state_listeners()
            || now.saturating_duration_since(self.last_tick) < TICK_MIN_INTERVAL
        {
            return PluginBatchOutcome::default();
        }
        self.last_tick = now;
        let st = ctx.player.state();
        let ev = HostEvent::Tick {
            generation: self.window.generation(),
            position: st.time_pos,
            duration: st.duration,
            paused: st.paused,
        };
        self.bind_current_file(ctx.player);
        let report = self.window.broadcast(&ev);
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    pub fn broadcast_state_changed<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let snap = self.playback_snapshot(ctx.player, ctx.store);
        let ev = HostEvent::StateChanged {
            generation: self.window.generation(),
            snapshot: snap,
        };
        self.bind_current_file(ctx.player);
        let report = self.window.broadcast(&ev);
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    pub fn apply_poll_report<P: PluginPlayer>(
        &mut self,
        mut ctx: PluginContext<'_, P>,
        report: PollReport,
        now: Instant,
    ) -> PluginBatchOutcome {
        self.apply_poll_report_ref(&mut ctx, report, now)
    }

    pub(super) fn apply_poll_report_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        report: PollReport,
        now: Instant,
    ) -> PluginBatchOutcome {
        let PollReport {
            actions,
            effects,
            notices,
            dirty,
            status_changed,
        } = report;
        let mut out = PluginBatchOutcome {
            dirty_slots: dirty,
            status_changed,
            ..Default::default()
        };
        if status_changed {
            out.domain.locations_reloaded = true;
        }
        for action in actions {
            if action.is_live(self.window.generation()) {
                out.merge(self.execute_window_action_ref(ctx, action, now));
            }
        }
        for effect in effects {
            let (nested, eff_out) = apply_ready_effect_step(
                &mut self.window,
                ctx.player,
                ctx.store,
                &mut self.overlay_frame,
                effect,
                now,
            );
            out.merge(eff_out);
            for rep in nested {
                out.merge(self.apply_poll_report_ref(ctx, rep, now));
            }
        }
        for notice in notices {
            out.merge(self.apply_notice_ref(ctx, notice, now));
        }
        if dirty.theme {
            out.domain.theme_changed = true;
        }
        if dirty.sources {
            out.merge(self.reconcile_active_source(ctx.active_source));
            if !ctx.active_source.is_local() {
                out.domain.catalog_changed = true;
            }
        }
        if dirty.modal {
            self.modal.on_dirty();
            out.domain.modal_changed = true;
        }
        out
    }

    fn execute_window_action_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        action: WindowAction,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        let pid = action.plugin_id().to_owned();
        match action.kind {
            ActionKind::Player(cmd) => {
                out.domain.playback_changed = true;
                if let Err(err) = ctx.player.execute(cmd) {
                    log::warn!("player command failed: {err:#}");
                    out.push_toast(player_command_error_label(&err), true);
                }
            }
            ActionKind::StepQueue { delta } => {
                out.domain.playback_changed = true;
                out.effects
                    .push(PluginSideEffect::StepMedia { delta, wrap: true });
            }
            ActionKind::ToggleFavorite => {
                if ctx.player.may_share_current_identity()
                    && let Some(uri) = ctx.player.current_media_uri()
                {
                    ctx.store.toggle_favorite(&uri);
                    out.domain.favorites_changed = true;
                    out.domain.catalog_changed = true;
                }
            }
            ActionKind::ShowToast { message } => {
                out.domain.modal_changed = true;
                if self.modal.clear_submitting_for(&pid)
                    && self.window.active_modal().is_some_and(|(p, _)| p == pid)
                {
                    self.modal.set_error(message);
                } else {
                    out.push_toast(message, false);
                }
            }
            ActionKind::RefreshSource { source_id } => {
                out.effects.push(PluginSideEffect::RefreshSource {
                    plugin_id: pid,
                    source_id,
                });
            }
            ActionKind::Enhancement(request) => {
                match execute_enhancement_step(&mut self.window, ctx.player, ctx.store, request) {
                    EnhancementStep::Done(step_out) => out.merge(step_out),
                    EnhancementStep::Effect(effect, step_out) => {
                        out.merge(step_out);
                        let (nested, eff_out) = apply_ready_effect_step(
                            &mut self.window,
                            ctx.player,
                            ctx.store,
                            &mut self.overlay_frame,
                            effect,
                            now,
                        );
                        out.merge(eff_out);
                        for rep in nested {
                            out.merge(self.apply_poll_report_ref(ctx, rep, now));
                        }
                    }
                    EnhancementStep::NestedReports(reports, step_out) => {
                        out.merge(step_out);
                        for rep in reports {
                            out.merge(self.apply_poll_report_ref(ctx, rep, now));
                        }
                    }
                }
            }
        }
        out
    }

    fn apply_notice_ref<P: PluginPlayer>(
        &mut self,
        ctx: &mut PluginContext<'_, P>,
        notice: Notice,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        match notice {
            Notice::Page {
                binding,
                generation,
                request,
                page,
            } => {
                if !binding.is_live() || generation.context != self.window.generation().context {
                    return out;
                }
                let pid = &binding.installed.plugin_id;
                let Some(append) = self.browse.take_page(
                    ctx.active_source,
                    pid,
                    request,
                    &page.page.source_id,
                    now,
                ) else {
                    return out;
                };
                if self
                    .browse
                    .view
                    .as_ref()
                    .is_none_or(|(p, v)| p != pid || *v != page.view)
                {
                    return out;
                }
                if let ActiveSource::Plugin(view) = ctx.active_source {
                    if view.plugin_id != *pid || view.source_id != page.page.source_id {
                        return out;
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
                        self.browse.pages.clear();
                        self.browse.source_filters = p.filters.clone();
                        self.browse.filter_values = p
                            .filters
                            .iter()
                            .filter_map(|f| {
                                f.selected
                                    .as_ref()
                                    .map(|value| (f.id.clone(), value.clone()))
                            })
                            .collect();
                    }
                    ctx.store.backfill_plugin_recent_titles(
                        pid,
                        &p.source_id,
                        p.entries.iter().map(|e| (e.id.as_str(), e.name.as_str())),
                    );
                    self.browse.page_binding = Some(binding);
                    self.browse.pages.push(page);
                    out.domain.catalog_changed = true;
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
                    || generation != self.window.generation()
                    || ticket.cancel.is_cancelled()
                {
                    return out;
                }
                let pid = &binding.installed.plugin_id;
                let Some(StreamReply::Requested {
                    source_id,
                    entry_id,
                    title: fallback_title,
                }) = self.browse.accept_stream(pid, Some(ticket.id), now)
                else {
                    return out;
                };
                if playback.title.trim().is_empty() || playback.title == DEFAULT_PLUGIN_MEDIA_TITLE
                {
                    playback.title = fallback_title
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| DEFAULT_PLUGIN_MEDIA_TITLE.into());
                }
                let requested_target = match &ticket.target {
                    RequestTarget::Resolve { source, entry, .. } => {
                        Some((source.as_str(), entry.as_str()))
                    }
                    _ => None,
                };
                let playlist_current = resolve_playlist_current(
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
                    _ => return out,
                };
                save_player_progress_sync(
                    ctx.player,
                    ctx.store,
                    ctx.settings.resume_playback,
                    self.stream.scoped_released,
                );
                let resume = if !private && ctx.settings.resume_playback && !playback.is_live {
                    ctx.store.resume(&uri)
                } else {
                    0.
                };
                let stream_variants = playback.variants.clone();
                let resolved_variant = playback.active_variant.clone();
                let settings = ctx.settings;
                let player = &mut *ctx.player;
                let admitted = self
                    .window
                    .admit_playback(pid, generation, playback, |scoped| {
                        player.open_scoped(scoped, uri.clone(), settings, resume)
                    });
                out.domain.playback_changed = true;
                match admitted {
                    Ok(Ok(())) => {
                        ctx.player.set_private_identity(private);
                        self.stream.scoped_released = false;
                        self.stream.stream_plugin = Some(pid.clone());
                        self.stream.playlist_current = playlist_current;
                        self.stream.playlist = playlist;
                        self.stream.playlist_binding = Some(binding.clone());
                        self.stream.active_stream_meta = match uri {
                            MediaUri::Plugin {
                                plugin_id,
                                source_id,
                                entry_id,
                            } => {
                                let entry_variants = match &*ctx.active_source {
                                    ActiveSource::Plugin(view)
                                        if view.plugin_id == plugin_id
                                            && view.source_id == source_id =>
                                    {
                                        self.browse
                                            .entries(&source_id)
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
                        out.effects.push(PluginSideEffect::MediaOpenSucceeded);
                        let status_report = self.window.notify_stream_status(pid, Some(ticket.id));
                        out.merge(self.apply_poll_report_ref(ctx, status_report, now));
                    }
                    Ok(Err(error)) => {
                        log::warn!("open_scoped failed for {pid}: {error:#}");
                        out.effects
                            .push(PluginSideEffect::MediaOpenFailed { show_toast: true });
                        out.push_toast("无法打开插件媒体，请检查网络或鉴权状态", true);
                    }
                    Err(code) => {
                        log::warn!("admit_playback rejected for {pid}: {code:?}");
                        let show_toast =
                            code != ErrorCode::Cancelled && code != ErrorCode::StaleHandle;
                        out.effects
                            .push(PluginSideEffect::MediaOpenFailed { show_toast });
                        if show_toast {
                            out.push_toast("无法打开插件媒体，请检查网络或鉴权状态", true);
                        }
                    }
                }
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
                let has_modal = self.window.active_modal().is_some();
                if self
                    .consent
                    .on_request_error(request, code, has_modal, &mut self.modal)
                {
                    out.domain.consent_changed = true;
                } else if request.is_none()
                    && let Some(pid) = plugin.as_deref()
                    && self.modal.clear_submitting_for(pid)
                {
                    out.domain.modal_changed = true;
                }
                let detail = message.unwrap_or_else(|| error_code_label(code).to_owned());
                let matched_source = if let (Some(pid), Some(id)) = (plugin.as_deref(), request) {
                    let (matched, err_out) =
                        self.handle_source_error(ctx.active_source, pid, id, detail.clone());
                    out.merge(err_out);
                    matched
                } else {
                    false
                };
                if !matched_source && should_fallback_error_toast(matched_source, code) {
                    out.push_toast(
                        format!("{} · {detail}", plugin.as_deref().unwrap_or("插件")),
                        false,
                    );
                }
            }
            Notice::Browser {
                binding,
                generation,
                url,
                ..
            } => {
                if binding.is_live() && generation == self.window.generation() {
                    out.effects.push(PluginSideEffect::OpenExternalUrl(url));
                }
            }
            Notice::Review(review) => {
                out.merge(self.consent.accept_review(&self.window, review, now));
            }
            Notice::Origin(origin) => {
                out.merge(self.consent.accept_origin(&self.window, origin));
            }
            Notice::OriginClosed(id) => {
                let has_modal = self.window.active_modal().is_some();
                out.merge(self.consent.close_origin(id, has_modal, &mut self.modal));
            }
            Notice::Installed(record) => {
                let has_modal = self.window.active_modal().is_some();
                out.merge(
                    self.consent
                        .on_installed(&record.plugin_id, has_modal, &mut self.modal),
                );
            }
            Notice::Uninstalled(plugin_id) => {
                out.merge(self.cleanup_playback_ref(ctx, Some(&plugin_id), None));
                out.merge(self.reconcile_active_source(ctx.active_source));
                out.domain.theme_changed = true;
                out.domain.uninstalled_changed = true;
                out.push_toast(format!("已删除插件 {plugin_id}"), false);
            }
            Notice::Commands(_) => {}
        }
        out
    }
}

fn player_command_error_label(err: &anyhow::Error) -> &'static str {
    use iris_core::player::PlayerActionError;
    if let Some(action) = err.downcast_ref::<PlayerActionError>() {
        return match action {
            PlayerActionError::NotSeekable => "当前媒体不支持设置 A-B 循环",
            PlayerActionError::InvalidAbRange => "A-B 循环区间无效或超出时长范围",
            PlayerActionError::NoVisualMedia => "LUT 预览仅适用于已加载的视频或图片",
            PlayerActionError::InvalidSubtitlePath => "本地字幕不支持远程或内部协议路径",
            PlayerActionError::SubtitleFileNotFound => "字幕文件不存在或不可读取",
            PlayerActionError::NonUnicodePath => "文件路径含有无法识别的字符，无法交给播放器",
        };
    }
    "当前状态无法执行此操作"
}
