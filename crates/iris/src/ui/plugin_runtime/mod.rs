//! Window-owned presentation of scoped plugin results and host consent.
mod annotations;
mod consent;
mod enhance;
mod notices;
pub(in crate::ui) use notices::describe_error_code;
mod overlay;
mod sources;
mod thumbnails;
mod timeline;
use super::IrisWindow;
use super::kit::text_buffer::TextInputBuffers;
use crate::assets::Icon;
use gpui::{Context, SharedString, Window};
use iris_api::PluginModal;
use iris_api::v2::{Generation, Id, source::PlaylistEntry};
use iris_core::plugin_v2::{
    hub::{Binding, OriginSummary, ReviewSummary},
    requests::AcceptedPage,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Maps a plugin-declared `SourceDescriptor::icon` name onto a controlled host [`Icon`].
pub(in crate::ui) fn source_icon_for_name(name: Option<&str>) -> Icon {
    match name.map(str::trim) {
        Some(n) if n.eq_ignore_ascii_case("globe") || n.eq_ignore_ascii_case("server") => {
            Icon::Globe
        }
        Some(n) if n.eq_ignore_ascii_case("folder") || n.eq_ignore_ascii_case("cloud") => {
            Icon::Folder
        }
        Some(n) if n.eq_ignore_ascii_case("link") => Icon::Link,
        Some(n) => Icon::from_name(n).unwrap_or(Icon::Puzzle),
        None => Icon::Puzzle,
    }
}

/// Consolidated async submission, inline error, and focus state for plugin modals.
#[derive(Default)]
pub(in crate::ui) struct PluginModalState {
    pub(in crate::ui) submitting: Option<(String, String)>,
    pub(in crate::ui) error: Option<SharedString>,
    pub(in crate::ui) focus_pending: bool,
}

impl PluginModalState {
    pub(in crate::ui) fn begin_submit(
        &mut self,
        buffers: &TextInputBuffers,
        pid: &str,
        modal: &PluginModal,
    ) -> Option<BTreeMap<String, String>> {
        if self
            .submitting
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == &modal.id)
        {
            return None;
        }
        self.error = None;
        self.submitting = Some((pid.to_string(), modal.id.clone()));
        Some(buffers.collect_modal_values(pid, modal))
    }

    pub(in crate::ui) fn clear_submitting_for(&mut self, pid: &str) -> bool {
        if self.submitting.as_ref().is_some_and(|(p, _)| p == pid) {
            self.submitting = None;
            return true;
        }
        false
    }

    pub(in crate::ui) fn cancel(&mut self, buffers: &mut TextInputBuffers) {
        self.submitting = None;
        self.error = None;
        buffers.clear_modal_fields();
    }

    pub(in crate::ui) fn on_dirty(&mut self) {
        self.submitting = None;
        self.error = None;
        self.focus_pending = true;
    }

    pub(in crate::ui) fn is_submitting(&self, pid: &str, modal_id: &str) -> bool {
        self.submitting
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == modal_id)
    }
}

pub(super) struct PluginView {
    pub deferred: Vec<(String, String)>,
    pub view: Option<(String, Id)>,
    pub pages: Vec<Arc<AcceptedPage>>,
    pub page_binding: Option<Arc<Binding>>,
    pub review: Option<Arc<ReviewSummary>>,
    pub review_created: Option<std::time::Instant>,
    pub origin: Option<Arc<OriginSummary>>,
    pub insecure_auth: bool,
    pub keep_identity: bool,
    pub consent_busy: Option<Id>,
    pub modal: PluginModalState,
    pub playlist: Vec<PlaylistEntry>,
    pub playlist_binding: Option<Arc<Binding>>,
    pub source_filters: Vec<iris_api::v2::source::SourceFilter>,
    pub filter_values: std::collections::BTreeMap<String, String>,
    pub view_timer: Option<((String, String, u32), gpui::Task<()>)>,
    thumbnails: thumbnails::Thumbnails,
    enhance: enhance::EnhancementView,
}
impl PluginView {
    pub fn new(deferred: Vec<(String, String)>) -> Self {
        Self {
            deferred,
            view: None,
            pages: Vec::new(),
            page_binding: None,
            review: None,
            review_created: None,
            origin: None,
            insecure_auth: false,
            keep_identity: false,
            consent_busy: None,
            modal: PluginModalState::default(),
            playlist: Vec::new(),
            playlist_binding: None,
            source_filters: Vec::new(),
            filter_values: std::collections::BTreeMap::new(),
            view_timer: None,
            thumbnails: thumbnails::Thumbnails::default(),
            enhance: enhance::EnhancementView::default(),
        }
    }
}
impl IrisWindow {
    pub(in crate::ui) fn plugin_source_icon(&self, plugin_id: &str, source_id: &str) -> Icon {
        let sources = self.plugins.media_sources();
        let exact = sources
            .iter()
            .find(|(pid, _, desc)| *pid == plugin_id && desc.id == source_id)
            .or_else(|| sources.iter().find(|(pid, _, _)| *pid == plugin_id));
        source_icon_for_name(exact.and_then(|(_, _, desc)| desc.icon.as_deref()))
    }

    pub(in crate::ui) fn plugin_source_badge(
        &self,
        plugin_id: &str,
        source_id: &str,
    ) -> SharedString {
        let sources = self.plugins.media_sources();
        let found = sources
            .iter()
            .copied()
            .find(|(pid, _, desc)| *pid == plugin_id && desc.id == source_id)
            .or_else(|| {
                sources
                    .iter()
                    .copied()
                    .find(|(pid, _, _)| *pid == plugin_id)
            });
        if let Some((_, pname, desc)) = found {
            if let Some(badge) = desc
                .badge
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
            {
                return badge.to_owned().into();
            }
            if !pname.trim().is_empty() {
                return pname.to_owned().into();
            }
        }
        "插件源".into()
    }
    pub(super) fn plugin_media_id(&self) -> Id {
        self.plugins.generation().media
    }
    pub(in crate::ui) fn bind_plugin_current_file(&self) {
        let path = self
            .active()
            .session
            .current()
            .map(std::path::Path::to_owned);
        for info in self.plugins.list() {
            if let Ok(access) = self.plugins.client().access(&info.id) {
                let _ = access.files.set_current_path(
                    self.plugin_media_id(),
                    access.generation,
                    path.clone(),
                );
            }
        }
    }
    pub(super) fn close_plugin_browse(&mut self) {
        if let Some((plugin, view)) = self.plugin_view.view.take() {
            let _ = self.plugins.client().close_view(&plugin, view);
        }
        self.catalog.cancel_browse();
        self.plugin_view.pages.clear();
        self.plugin_view.page_binding = None;
        self.plugin_view.source_filters.clear();
        self.plugin_view.filter_values.clear();
        self.plugin_view.thumbnails = thumbnails::Thumbnails::default();
    }
    pub(super) fn advance_plugin_media(&mut self, cx: &mut Context<Self>) -> bool {
        self.plugin_view.enhance.annotations.selections.clear();
        std::mem::take(&mut self.plugin_view.enhance.annotations.cancel).cancel();
        if self.plugins.client().window_id() == Id(0) {
            return true;
        }
        let old = self.plugins.generation();
        let Some(media) = old.media.0.checked_add(1) else {
            return false;
        };
        match self.plugins.client().advance(Generation {
            media: Id(media),
            seek: Id(0),
            context: old.context,
        }) {
            Ok(()) => {
                // The previous StreamSession is already cancelled by `advance`;
                // stop and release the old relay immediately so its Http,
                // RelayWorker, and PlaybackSession quotas are freed before
                // resolving the next video.
                self.active_mut().session.release_scoped();
                if let Some(origin) = self.plugin_view.origin.take()
                    && self.plugin_view.consent_busy == Some(origin.id)
                {
                    self.plugin_view.consent_busy = None;
                }
                self.plugin_view.playlist.clear();
                self.plugin_view.playlist_binding = None;
                self.plugin_view.enhance.media_announced = None;
                self.plugin_view.enhance.overlay_generation = self.plugins.generation();
                self.reset_timed_overlays();
                self.plugin_view.enhance.pending.clear();
                self.plugin_view.enhance.lut_owner = None;
                self.plugin_view.enhance.timeline = None;
                true
            }
            Err(error) => {
                log::warn!("advance_plugin_media failed: {error:?}");
                self.show_error_toast(
                    format!("媒体切换暂未完成 · {}", describe_error_code(error)),
                    cx,
                );
                false
            }
        }
    }
    pub(super) fn advance_plugin_seek(&mut self, cx: &mut Context<Self>) {
        // Playback seeking does not revoke unrelated directory, configuration
        // or login work in ControlStore. StreamSession owns its read epoch;
        // timed overlays carry an independent inner generation in their events.
        let Some(seek) = self
            .plugin_view
            .enhance
            .overlay_generation
            .seek
            .0
            .checked_add(1)
        else {
            return;
        };
        self.plugin_view.enhance.overlay_generation.seek = Id(seek);
        self.reset_timed_overlays();
        self.plugin_view
            .enhance
            .pending
            .retain(|(_, key), _| !key.starts_with("overlay:"));
        if let Some(session) =
            iris_core::plugin_v2::stream::prepare::stream_session(&self.active().session)
        {
            let plugin = self
                .plugins
                .list()
                .iter()
                .find(|plugin| {
                    plugin
                        .binding
                        .as_ref()
                        .is_some_and(|b| b.installed.installation_id == session.factory.owner())
                })
                .map(|plugin| plugin.id.clone());
            if let Some(plugin) = plugin {
                self.notify_stream_status(&plugin, None, cx);
            }
        }
    }
    pub(super) fn reconcile_plugin_lifetimes(&mut self, cx: &mut Context<Self>) {
        self.reconcile_enhancements(cx);
        if self
            .plugin_view
            .playlist_binding
            .as_ref()
            .is_some_and(|b| !b.is_live())
        {
            self.plugin_view.playlist.clear();
            self.plugin_view.playlist_binding = None;
        }
        if self
            .plugin_view
            .page_binding
            .as_ref()
            .is_some_and(|b| !b.is_live())
        {
            self.close_plugin_browse();
            self.catalog.active_source = super::state::ActiveSource::Local;
            self.rebuild_directory_rows();
        }
        if let Some(review) = self
            .plugin_view
            .review
            .as_ref()
            .filter(|_| {
                self.plugin_view
                    .review_created
                    .is_some_and(|t| t.elapsed().as_secs() >= 300)
            })
            .cloned()
        {
            let _ = self.plugins.client().dismiss_review(review.id);
            self.plugin_view.review = None;
            self.plugin_view.review_created = None;
            if self.plugin_view.consent_busy == Some(review.id) {
                self.plugin_view.consent_busy = None;
            }
            cx.notify();
        }
        if self
            .plugin_view
            .origin
            .as_ref()
            .is_some_and(|r| !r.binding.is_live() || r.generation != self.plugins.generation())
        {
            if let Some(origin) = self.plugin_view.origin.take()
                && self.plugin_view.consent_busy == Some(origin.id)
            {
                self.plugin_view.consent_busy = None;
            }
            cx.notify();
        }
    }
    pub(super) fn run_deferred_plugin_commands(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = std::mem::take(&mut self.plugin_view.deferred);
        for (plugin, command) in pending {
            if self.plugins.client().is_ready(&plugin) {
                self.invoke_plugin_command(&plugin, &command, window, cx);
            } else if self
                .plugins
                .list()
                .iter()
                .find(|p| p.id == plugin)
                .is_none_or(|p| {
                    p.enabled
                        && !matches!(p.status, iris_core::plugin_v2::hub::PluginStatus::Failed(_))
                })
            {
                self.plugin_view.deferred.push((plugin, command));
            }
        }
    }
    pub(super) fn announce_plugin_media_if_loaded(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.active().session.state().loaded {
            return false;
        }
        let media = self.plugin_media_id();
        if self.plugin_view.enhance.media_announced == Some(media) {
            return false;
        }
        self.plugin_view.enhance.media_announced = Some(media);
        let snap = self.plugin_snapshot();
        self.broadcast_plugin_event(
            iris_api::v2::HostEvent::MediaOpened {
                generation: self.plugins.generation(),
                snapshot: snap,
            },
            None,
            cx,
        )
    }
    fn active_view_timer_target(&self) -> Option<(String, String, u32)> {
        if !self.has_host_consent()
            && let Some((pid, modal)) = self.plugins.active_modal()
            && let Some(timer) = &modal.timer
            && !timer.action_id.is_empty()
        {
            return Some((
                pid.to_string(),
                timer.action_id.clone(),
                timer.interval_ms.clamp(1_000, 300_000),
            ));
        }
        for (pid, panel) in self.plugins.sidebar_panels() {
            if let Some(timer) = &panel.timer
                && !timer.action_id.is_empty()
            {
                return Some((
                    pid.to_string(),
                    timer.action_id.clone(),
                    timer.interval_ms.clamp(1_000, 300_000),
                ));
            }
        }
        None
    }
    pub(super) fn sync_view_timer(&mut self, cx: &mut Context<Self>) {
        let next = self.active_view_timer_target();
        if self.plugin_view.view_timer.as_ref().map(|(k, _)| k) == next.as_ref() {
            return;
        }
        let Some((pid, act, interval_ms)) = next else {
            self.plugin_view.view_timer = None;
            return;
        };
        let key = (pid.clone(), act.clone(), interval_ms);
        let expected = key.clone();
        let interval = std::time::Duration::from_millis(u64::from(interval_ms));
        let task = cx.spawn(async move |this, cx| {
            for _ in 0..120 {
                cx.background_executor().timer(interval).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.active_view_timer_target().as_ref() != Some(&expected) {
                            this.plugin_view.view_timer = None;
                            return false;
                        }
                        let snap = this.plugin_snapshot();
                        let report = this.plugins.send_to(
                            &pid,
                            iris_api::v2::HostEvent::Action {
                                id: act.clone(),
                                value: None,
                                snapshot: Some(snap),
                            },
                        );
                        if this.apply_poll_report(report, None, cx) {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
        self.plugin_view.view_timer = Some((key, task));
    }
}
