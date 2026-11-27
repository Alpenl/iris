//! Window-owned presentation of scoped plugin results and host consent.
pub(in crate::ui) mod annotations;
mod consent;
pub(in crate::ui) use consent::render_host_consent;
mod enhance;
mod notices;
pub(in crate::ui) use notices::describe_error_code;
pub(in crate::ui) mod overlay;
pub(in crate::ui) mod sources;
pub(in crate::ui) use sources::remote_search_query;
mod thumbnails;
pub(in crate::ui) mod timeline;
use super::AppModel;
use super::kit::text_buffer::TextInputBuffers;
use super::views::DirtyRegions;
use crate::assets::Icon;
use gpui::{Context, SharedString, Window};
use iris_api::v2::{Capability, ErrorCode, Id, source::PlaylistEntry};
use iris_api::{PluginModal, SidebarPanel};
use iris_core::preset::SidebarMode;
use iris_plugin_host::{
    hub::{Binding, OriginSummary, PluginStatus, ReviewSummary},
    overlay::OverlayFrame,
    requests::AcceptedPage,
};
use std::collections::{BTreeMap, BTreeSet};
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

    pub(in crate::ui) fn clear_feedback(&mut self) {
        self.submitting = None;
        self.error = None;
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ViewTimerSlot {
    Modal(String),
    Sidebar(String),
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
    pub consent_busy: BTreeSet<Id>,
    pub modal: PluginModalState,
    pub stream_plugin: Option<String>,
    pub scoped_released: bool,
    pub playlist: Vec<PlaylistEntry>,
    pub playlist_binding: Option<Arc<Binding>>,
    pub playlist_current: Option<(String, String)>,
    pub source_filters: Vec<iris_api::v2::source::SourceFilter>,
    pub filter_values: std::collections::BTreeMap<String, String>,
    pub view_timers: BTreeMap<ViewTimerSlot, ((String, u32), gpui::Task<()>)>,
    thumbnails: thumbnails::Thumbnails,
    pub(super) overlay_frame: Option<Arc<OverlayFrame>>,
    pub(super) overlay_wake: Option<gpui::Task<()>>,
    pub(super) glyphs: overlay::GlyphCache,
    pub(super) annotations: annotations::AnnotationView,
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
            consent_busy: BTreeSet::new(),
            modal: PluginModalState::default(),
            stream_plugin: None,
            scoped_released: false,
            playlist: Vec::new(),
            playlist_binding: None,
            playlist_current: None,
            source_filters: Vec::new(),
            filter_values: std::collections::BTreeMap::new(),
            view_timers: BTreeMap::new(),
            thumbnails: thumbnails::Thumbnails::default(),
            overlay_frame: None,
            overlay_wake: None,
            glyphs: overlay::GlyphCache::default(),
            annotations: annotations::AnnotationView::default(),
        }
    }
}

impl AppModel {
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

    pub(in crate::ui) fn plugin_source_entries<'a>(
        &'a self,
        source_id: &'a str,
    ) -> impl Iterator<Item = &'a iris_api::v2::source::SourceEntry> {
        self.plugin_view
            .pages
            .iter()
            .filter(move |p| p.page.source_id == source_id)
            .flat_map(|p| &p.page.entries)
    }

    pub(in crate::ui) fn restore_modal_focus_if_unblocked(&mut self) {
        if !self.has_host_consent() && self.plugins.active_modal().is_some() {
            self.plugin_view.modal.focus_pending = true;
        }
    }

    pub(in crate::ui) fn bind_plugin_current_file(&self) {
        let path = self
            .active()
            .session
            .current()
            .map(std::path::Path::to_owned);
        self.plugins.set_current_local_file(path);
    }

    pub(in crate::ui) fn precheck_plugin_capability(
        &self,
        plugin_id: &str,
        capability: Capability,
    ) -> Result<(), ErrorCode> {
        self.plugins.precheck_capability(plugin_id, capability)
    }

    pub(super) fn close_plugin_browse(&mut self) {
        if let Some((plugin, view)) = self.plugin_view.view.take() {
            let _ = self.plugins.close_view(&plugin, view);
        }
        self.catalog.cancel_browse();
        self.plugin_view.pages.clear();
        self.plugin_view.page_binding = None;
        self.plugin_view.source_filters.clear();
        self.plugin_view.filter_values.clear();
        self.plugin_view.thumbnails = thumbnails::Thumbnails::default();
    }

    pub(super) fn advance_plugin_media(&mut self, cx: &mut Context<Self>) -> bool {
        std::mem::take(&mut self.plugin_view.annotations.cancel).cancel();
        let connected = self.plugins.is_connected();
        match self.plugins.advance_media() {
            Ok(()) => {
                // The previous StreamSession is already cancelled and cleared by
                // `advance_media`; stop and release the old relay immediately so its
                // Http, RelayWorker, and PlaybackSession quotas are freed before
                // resolving the next video.
                let had_scoped = self.active().session.scoped_stream().is_some();
                self.active_mut().session.release_scoped();
                if had_scoped {
                    self.plugin_view.scoped_released = true;
                }
                self.plugin_view.stream_plugin = None;
                if connected && let Some(origin) = self.plugin_view.origin.take() {
                    self.plugin_view.consent_busy.remove(&origin.id);
                    self.restore_modal_focus_if_unblocked();
                    self.sync_view_timer(cx);
                }
                self.plugin_view.playlist.clear();
                self.plugin_view.playlist_binding = None;
                self.plugin_view.playlist_current = None;
                self.reset_timed_overlays();
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
        self.plugin_view.overlay_frame = None;
        self.plugin_view.glyphs.clear();
        self.plugin_view.overlay_wake = None;
        if let Some(report) = self.plugins.advance_overlay_seek() {
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
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
            self.plugin_view.playlist_current = None;
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
            self.notify_sidebar(cx);
        }
        if let Some(pid) = self
            .catalog
            .active_stream_meta
            .as_ref()
            .and_then(|m| m.plugin_id.clone())
            && !self
                .plugins
                .list()
                .iter()
                .any(|p| p.id == pid && p.enabled && p.status == PluginStatus::Ready)
        {
            self.catalog
                .clear_stream_meta_for(&mut self.overlays, Some(&pid), None);
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
            let _ = self.plugins.dismiss_review(review.id);
            self.plugin_view.review = None;
            self.plugin_view.review_created = None;
            self.plugin_view.consent_busy.remove(&review.id);
            self.restore_modal_focus_if_unblocked();
            self.sync_view_timer(cx);
            self.notify_root(cx);
        }
        if self
            .plugin_view
            .origin
            .as_ref()
            .is_some_and(|r| !r.binding.is_live() || r.generation != self.plugins.generation())
        {
            if let Some(origin) = self.plugin_view.origin.take() {
                self.plugin_view.consent_busy.remove(&origin.id);
            }
            self.restore_modal_focus_if_unblocked();
            self.sync_view_timer(cx);
            self.notify_root(cx);
        }
    }

    pub(super) fn run_deferred_plugin_commands(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = std::mem::take(&mut self.plugin_view.deferred);
        for (plugin, command) in pending {
            if self.plugins.is_ready(&plugin) {
                self.invoke_plugin_command(&plugin, &command, window, cx);
            } else if self
                .plugins
                .list()
                .iter()
                .find(|p| p.id == plugin)
                .is_none_or(|p| p.enabled && !matches!(p.status, PluginStatus::Failed(_)))
            {
                self.plugin_view.deferred.push((plugin, command));
            }
        }
    }

    pub(super) fn announce_plugin_media_if_loaded(
        &mut self,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        if !self.active().session.state().loaded {
            return DirtyRegions::NONE;
        }
        self.bind_plugin_current_file();
        let snap = self.plugin_snapshot();
        let Some(report) = self.plugins.announce_media_opened(snap) else {
            return DirtyRegions::NONE;
        };
        self.apply_poll_report(report, None, cx)
    }

    fn desired_view_timers(&self) -> BTreeMap<ViewTimerSlot, (String, u32)> {
        let panels = self.plugins.sidebar_panels();
        collect_view_timer_targets(
            self.has_host_consent(),
            self.sidebar_is_visible() && self.catalog.sidebar_mode == SidebarMode::Plugins,
            self.plugins.active_modal(),
            &panels,
        )
    }

    pub(super) fn sync_view_timer(&mut self, cx: &mut Context<Self>) {
        let desired = self.desired_view_timers();
        self.plugin_view
            .view_timers
            .retain(|slot, (spec, _)| desired.get(slot) == Some(spec));
        for (slot, (act, interval_ms)) in desired {
            if self.plugin_view.view_timers.contains_key(&slot) {
                continue;
            }
            let pid = match &slot {
                ViewTimerSlot::Modal(p) | ViewTimerSlot::Sidebar(p) => p.clone(),
            };
            let expected_slot = slot.clone();
            let expected_spec = (act.clone(), interval_ms);
            let interval = std::time::Duration::from_millis(u64::from(interval_ms));
            let task = cx.spawn(async move |this, cx| {
                let executor = cx.background_executor().clone();
                run_view_timer(
                    || executor.timer(interval),
                    || {
                        this.update(cx, |this, cx| {
                            if this.desired_view_timers().get(&expected_slot)
                                != Some(&expected_spec)
                            {
                                this.plugin_view.view_timers.remove(&expected_slot);
                                return false;
                            }
                            let snap = this.plugin_snapshot();
                            let report = this.plugins.send_to(
                                &pid,
                                iris_api::v2::HostEvent::Action {
                                    id: expected_spec.0.clone(),
                                    value: None,
                                    snapshot: Some(snap),
                                },
                            );
                            let regions = this.apply_poll_report(report, None, cx);
                            if regions.any() {
                                this.notify_regions(regions, cx);
                            }
                            true
                        })
                        .unwrap_or(false)
                    },
                )
                .await;
            });
            self.plugin_view
                .view_timers
                .insert(slot, ((act, interval_ms), task));
        }
    }
}

pub(super) fn collect_view_timer_targets(
    has_host_consent: bool,
    sidebar_plugins_visible: bool,
    active_modal: Option<(&str, &PluginModal)>,
    sidebar_panels: &[(&str, &SidebarPanel)],
) -> BTreeMap<ViewTimerSlot, (String, u32)> {
    let mut out = BTreeMap::new();
    if has_host_consent {
        return out;
    }
    let mut modal_action: Option<(&str, &str)> = None;
    if let Some((pid, modal)) = active_modal
        && let Some(timer) = &modal.timer
        && !timer.action_id.is_empty()
    {
        modal_action = Some((pid, timer.action_id.as_str()));
        out.insert(
            ViewTimerSlot::Modal(pid.to_owned()),
            (
                timer.action_id.clone(),
                timer.interval_ms.clamp(1_000, 300_000),
            ),
        );
    }
    if sidebar_plugins_visible {
        for &(pid, panel) in sidebar_panels {
            if let Some(timer) = &panel.timer
                && !timer.action_id.is_empty()
                && modal_action != Some((pid, timer.action_id.as_str()))
            {
                out.insert(
                    ViewTimerSlot::Sidebar(pid.to_owned()),
                    (
                        timer.action_id.clone(),
                        timer.interval_ms.clamp(1_000, 300_000),
                    ),
                );
            }
        }
    }
    out
}

async fn run_view_timer<F: std::future::Future<Output = ()>>(
    mut wait: impl FnMut() -> F,
    mut tick: impl FnMut() -> bool,
) {
    loop {
        wait().await;
        if !tick() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ViewTimerSlot, collect_view_timer_targets, run_view_timer};
    use futures::{FutureExt, executor::block_on};
    use iris_api::{PluginModal, SidebarPanel, ViewTimer};
    use std::{cell::Cell, future};

    #[test]
    fn view_timer_continues_past_120_ticks_until_target_disappears() {
        let elapsed = Cell::new(0);
        let callbacks = Cell::new(0);
        block_on(run_view_timer(
            || {
                elapsed.set(elapsed.get() + 1);
                future::ready(())
            },
            || {
                if elapsed.get() > 600 {
                    return false;
                }
                callbacks.set(callbacks.get() + 1);
                true
            },
        ));
        assert_eq!(callbacks.get(), 600);
        assert_eq!(elapsed.get(), 601);
    }

    #[test]
    fn dropping_view_timer_while_waiting_does_not_dispatch() {
        let callbacks = Cell::new(0);
        let task = run_view_timer(future::pending::<()>, || {
            callbacks.set(callbacks.get() + 1);
            true
        });
        assert!(task.now_or_never().is_none());
        assert_eq!(callbacks.get(), 0);
    }

    #[test]
    fn view_timer_targets_respect_consent_sidebar_visibility_and_multiple_panels() {
        let panel_a = SidebarPanel {
            title: "A".into(),
            sections: Vec::new(),
            timer: Some(ViewTimer {
                interval_ms: 2_000,
                action_id: "tick-a".into(),
            }),
        };
        let panel_b = SidebarPanel {
            title: "B".into(),
            sections: Vec::new(),
            timer: Some(ViewTimer {
                interval_ms: 250,
                action_id: "tick-b".into(),
            }),
        };
        let panels = [("plugin.a", &panel_a), ("plugin.b", &panel_b)];

        // Hidden sidebar or non-Plugins mode dispatches zero sidebar timers.
        assert!(collect_view_timer_targets(false, false, None, &panels).is_empty());

        // Visible Plugins sidebar runs all panel timers concurrently without starving secondary plugins.
        let both = collect_view_timer_targets(false, true, None, &panels);
        assert_eq!(both.len(), 2);
        assert_eq!(
            both.get(&ViewTimerSlot::Sidebar("plugin.a".into())),
            Some(&("tick-a".to_owned(), 2_000))
        );
        assert_eq!(
            both.get(&ViewTimerSlot::Sidebar("plugin.b".into())),
            Some(&("tick-b".to_owned(), 1_000))
        );

        // Host consent pauses underlying modal/sidebar timers and resumes them on cancel/expiry.
        let modal = PluginModal {
            id: "qr".into(),
            title: "扫码登录".into(),
            subtitle: None,
            widgets: Vec::new(),
            submit_label: None,
            cancel_label: None,
            timer: Some(ViewTimer {
                interval_ms: 1_500,
                action_id: "poll-qr".into(),
            }),
        };
        assert!(
            collect_view_timer_targets(true, true, Some(("plugin.a", &modal)), &panels).is_empty()
        );
        let resumed = collect_view_timer_targets(false, false, Some(("plugin.a", &modal)), &panels);
        assert_eq!(
            resumed.get(&ViewTimerSlot::Modal("plugin.a".into())),
            Some(&("poll-qr".to_owned(), 1_500))
        );
    }
}
