//! Window-owned presentation of scoped plugin results and host consent.
mod annotations;
mod byob;
mod byob_consent;
mod consent;
mod enhance;
mod export;
mod notices;
pub(in crate::ui) use notices::describe_error_code;
mod overlay;
mod sources;
mod thumbnails;
mod timeline;
use super::IrisWindow;
use gpui::{Context, Window};
use iris_api::v2::{Generation, Id, source::PlaylistEntry};
use iris_core::plugin_v2::{
    hub::{Binding, OriginSummary, ReviewSummary},
    requests::AcceptedPage,
};
use std::sync::Arc;

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
    pub playlist: Vec<PlaylistEntry>,
    pub playlist_binding: Option<Arc<Binding>>,
    pub source_filters: Vec<iris_api::v2::source::SourceFilter>,
    pub filter_values: std::collections::BTreeMap<String, String>,
    pub qr_poll: Option<((String, String), gpui::Task<()>)>,
    thumbnails: thumbnails::Thumbnails,
    tools: byob::ToolView,
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
            playlist: Vec::new(),
            playlist_binding: None,
            source_filters: Vec::new(),
            filter_values: std::collections::BTreeMap::new(),
            qr_poll: None,
            thumbnails: thumbnails::Thumbnails::default(),
            tools: byob::ToolView::default(),
            enhance: enhance::EnhancementView::default(),
        }
    }
}
impl IrisWindow {
    pub(super) fn plugin_media_id(&self) -> Id {
        self.plugins.generation().media
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
        if let Some(session) = self.active().session.plugin_session() {
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
        if self.plugin_view.tools.input.as_ref().is_some_and(|r| {
            !r.binding.is_live()
                || r.access.generation != self.plugins.generation()
                || r.created.elapsed().as_secs() >= 300
        }) {
            self.finish_input_consent(false, cx);
        }
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
        if self
            .plugin_view
            .tools
            .review
            .as_ref()
            .is_some_and(|(_, created)| created.elapsed().as_secs() >= 300)
        {
            self.plugin_view.tools.review = None;
            cx.notify();
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
    fn active_qr_poll_target(&self) -> Option<(String, String)> {
        if !self.has_host_consent()
            && let Some((pid, modal)) = self.plugins.active_modal()
        {
            for widget in &modal.widgets {
                if let iris_api::Widget::QrCode {
                    matrix,
                    poll_action_id: Some(act),
                    ..
                } = widget
                    && !matrix.is_empty()
                    && !act.is_empty()
                {
                    return Some((pid.to_string(), act.clone()));
                }
            }
        }
        for (pid, panel) in self.plugins.sidebar_panels() {
            for section in &panel.sections {
                for widget in &section.widgets {
                    if let iris_api::Widget::QrCode {
                        matrix,
                        poll_action_id: Some(act),
                        ..
                    } = widget
                        && !matrix.is_empty()
                        && !act.is_empty()
                    {
                        return Some((pid.to_string(), act.clone()));
                    }
                }
            }
        }
        None
    }
    pub(super) fn sync_qr_poll_timer(&mut self, cx: &mut Context<Self>) {
        let next = self.active_qr_poll_target();
        if self.plugin_view.qr_poll.as_ref().map(|(k, _)| k) == next.as_ref() {
            return;
        }
        let Some((pid, act)) = next else {
            self.plugin_view.qr_poll = None;
            return;
        };
        let key = (pid.clone(), act.clone());
        let expected = key.clone();
        let task = cx.spawn(async move |this, cx| {
            for _ in 0..120 {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.active_qr_poll_target().as_ref() != Some(&expected) {
                            this.plugin_view.qr_poll = None;
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
        self.plugin_view.qr_poll = Some((key, task));
    }
}
