//! Media enhancement application (`ReadyEffect`), timed-overlay frame stepping,
//! and host/plugin annotation coordination for [`super::PluginController`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_api::v2::{
    ErrorCode, Generation,
    enhance::{HeatRange, SkipRange},
};
use iris_base::net::http_client::HttpCancel;
use iris_core::media_uri::MediaUri;
use iris_core::store::{
    AnnotationEdit as StoreAnnotationEdit, AnnotationSnapshot, AnnotationTarget, Store,
};
use iris_plugin_host::{
    EnhancementOutcome, EnhancementRequest, HostAnnotationJob, OverlayFrame, OverlayStep,
    PluginHost, PollReport, ReadyEffect, WindowSession, annotation_error_code,
};

use super::ports::{PluginBatchOutcome, PluginPlayer, PluginSideEffect, error_code_label};

/// Prepared arguments needed to spawn a background host annotation job on a UI executor.
pub type PendingHostAnnotation = (
    AnnotationTarget,
    MediaUri,
    Option<StoreAnnotationEdit>,
    HttpCancel,
    HostAnnotationJob,
);

/// Returns `true` when clicking a rendered skip range is still valid for the current media generation.
pub fn can_seek_skip_range(
    rendered_generation: Generation,
    current_generation: Generation,
    has_timeline_ranges: bool,
) -> bool {
    current_generation == rendered_generation && has_timeline_ranges
}

#[derive(Default)]
pub(super) struct AnnotationJobState {
    pub cancel: HttpCancel,
    pub busy: bool,
    pub refresh_pending: bool,
}

impl AnnotationJobState {
    pub fn cancel_in_flight(&mut self) {
        std::mem::take(&mut self.cancel).cancel();
    }

    pub fn begin(
        &mut self,
        host: &PluginHost,
        player: &impl PluginPlayer,
        store: &Store,
        edit: Option<StoreAnnotationEdit>,
    ) -> Result<Option<PendingHostAnnotation>, PluginBatchOutcome> {
        let Some(uri) = player
            .may_share_current_identity()
            .then(|| player.current_media_uri())
            .flatten()
        else {
            return Ok(None);
        };
        if self.busy {
            if edit.is_none() {
                self.refresh_pending = true;
                return Ok(None);
            }
            let mut out = PluginBatchOutcome::default();
            out.push_toast("标记正在保存，请稍后重试", false);
            return Err(out);
        }
        let setup = (|| {
            let target = store.annotation_target().map_err(annotation_error_code)?;
            let host_job = host.begin_host_annotation()?;
            Ok::<_, ErrorCode>((target, host_job))
        })();
        match setup {
            Ok((target, host_job)) => {
                self.busy = true;
                let cancel = self.cancel.clone();
                Ok(Some((target, uri, edit, cancel, host_job)))
            }
            Err(error) => {
                let mut out = PluginBatchOutcome::default();
                out.push_toast(
                    format!("个人标记不可用 · {}", error_code_label(error)),
                    false,
                );
                Err(out)
            }
        }
    }

    pub fn finish(
        &mut self,
        store: &mut Store,
        saving: bool,
        result: Result<AnnotationSnapshot, ErrorCode>,
    ) -> (bool, PluginBatchOutcome) {
        self.busy = false;
        let mut out = PluginBatchOutcome::default();
        out.domain.annotations_changed = true;
        match result {
            Ok(snapshot) => {
                let _ = store.apply_annotation_snapshot(snapshot);
                if saving {
                    out.push_toast("个人标记已保存", false);
                }
            }
            Err(ErrorCode::Cancelled) => {}
            Err(error) => {
                out.push_toast(
                    format!("个人标记未更新 · {}", error_code_label(error)),
                    false,
                );
            }
        }
        let refresh = std::mem::take(&mut self.refresh_pending);
        (refresh, out)
    }
}

impl Drop for AnnotationJobState {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(super) enum EnhancementStep {
    Done(PluginBatchOutcome),
    Effect(ReadyEffect, PluginBatchOutcome),
    NestedReports(Vec<PollReport>, PluginBatchOutcome),
}

pub(super) fn execute_enhancement_step<P: PluginPlayer>(
    window: &mut WindowSession,
    player: &P,
    store: &Store,
    request: EnhancementRequest,
) -> EnhancementStep {
    let (media_loaded, position_ms) = {
        let st = player.state();
        (st.loaded, (st.time_pos.max(0.) * 1000.) as u64)
    };
    let mut out = PluginBatchOutcome::default();
    out.domain.playback_changed = true;
    match window.execute_enhancement(request, media_loaded, position_ms, || {
        store.annotation_target()
    }) {
        EnhancementOutcome::Queued | EnhancementOutcome::Noop => EnhancementStep::Done(out),
        EnhancementOutcome::Immediate(effect) => EnhancementStep::Effect(effect, out),
        EnhancementOutcome::Rejected {
            code,
            superseded,
            report,
        } => {
            if superseded {
                log::debug!("ignoring superseded plugin enhancement action: {code:?}");
            } else {
                log::warn!("plugin enhancement action failed: {code:?}");
                out.push_toast(
                    format!("插件操作未完成 · {}", error_code_label(code)),
                    false,
                );
            }
            EnhancementStep::NestedReports(vec![report], out)
        }
    }
}

pub(super) fn apply_ready_effect_step<P: PluginPlayer>(
    window: &mut WindowSession,
    player: &mut P,
    store: &mut Store,
    overlay_frame: &mut Option<Arc<OverlayFrame>>,
    effect: ReadyEffect,
    now: Instant,
) -> (Vec<PollReport>, PluginBatchOutcome) {
    let mut out = PluginBatchOutcome::default();
    let mut reports = Vec::new();
    match effect {
        ReadyEffect::Timeline(lease) => {
            let chapters = lease.chapters();
            let owner = lease.owner().to_owned();
            match player.set_plugin_timeline(owner, &chapters) {
                Ok(()) => {
                    window.commit_timeline(lease);
                    out.domain.timeline_changed = true;
                }
                Err(_) => {
                    log::warn!(
                        "plugin enhancement action failed: {:?}",
                        ErrorCode::BadState
                    );
                    out.push_toast(
                        format!("插件操作未完成 · {}", error_code_label(ErrorCode::BadState)),
                        false,
                    );
                }
            }
        }
        ReadyEffect::Attachment(attachment) => {
            out.domain.track_changed = true;
            if let Err(error) = player.attach_plugin_track(attachment.into_external_track()) {
                out.push_toast(format!("媒体轨道添加失败 · {error}"), false);
            }
        }
        ReadyEffect::MonitorLut(ready_lut) => {
            let (owner, filter) = ready_lut.into_filter();
            out.domain.lut_changed = true;
            match player.set_monitor_lut(Some(filter)) {
                Ok(()) => window.commit_lut_owner(owner),
                Err(error) => out.push_toast(format!("LUT 未应用 · {error}"), false),
            }
        }
        ReadyEffect::ClearMonitorLut { owner } => match player.set_monitor_lut(None) {
            Ok(()) => {
                window.confirm_lut_cleared(&owner);
                out.domain.lut_changed = true;
            }
            Err(_) => {
                log::warn!("plugin enhancement action failed: {:?}", ErrorCode::IoError);
                out.push_toast(
                    format!("插件操作未完成 · {}", error_code_label(ErrorCode::IoError)),
                    false,
                );
            }
        },
        ReadyEffect::TimedOverlay(_ready) => {
            out.effects.push(PluginSideEffect::ClearOverlayWake);
            out.domain.overlay_changed = true;
            let (step_reports, step_out) =
                step_overlay_frame_inner(window, player.state().loaded, overlay_frame, now);
            reports.extend(step_reports);
            out.merge(step_out);
        }
        ReadyEffect::StreamUpdated(ready) => {
            reports.push(window.notify_stream_status(&ready.plugin_id, None));
        }
        ReadyEffect::Annotations(ready) => {
            let ready = *ready;
            if let Some(snapshot) = ready.snapshot {
                let _ = store.apply_annotation_snapshot(snapshot);
            }
            out.domain.annotations_changed = true;
            reports.push(window.send_to(&ready.plugin_id, ready.event));
        }
        ReadyEffect::Failed { code, .. } => {
            log::warn!("plugin enhancement job failed: {code:?}");
            out.push_toast(
                format!("插件操作未完成 · {}", error_code_label(code)),
                false,
            );
        }
    }
    (reports, out)
}

pub(super) fn step_overlay_frame_inner(
    window: &mut WindowSession,
    media_loaded: bool,
    overlay_frame: &mut Option<Arc<OverlayFrame>>,
    now: Instant,
) -> (Vec<PollReport>, PluginBatchOutcome) {
    let mut out = PluginBatchOutcome::default();
    match window.step_overlay_frame(media_loaded, now) {
        OverlayStep::Empty => {
            if overlay_frame.take().is_some() {
                out.domain.overlay_changed = true;
            }
            (Vec::new(), out)
        }
        OverlayStep::Busy { retry_after } => {
            if let Some(delay) = retry_after {
                out.effects
                    .push(PluginSideEffect::ScheduleOverlayWake { delay });
            }
            (Vec::new(), out)
        }
        OverlayStep::Updated { frame, reports } => {
            let Some(frame) = frame else {
                if overlay_frame.take().is_some() {
                    out.domain.overlay_changed = true;
                }
                return (reports, out);
            };
            if !frame.animate
                && let Some(delay) = frame.next_change
            {
                out.effects.push(PluginSideEffect::ScheduleOverlayWake {
                    delay: delay.max(Duration::from_millis(1)),
                });
            }
            let had_visible = overlay_frame
                .as_ref()
                .is_some_and(|prev| !prev.cues.is_empty());
            let has_visible = !frame.cues.is_empty();
            *overlay_frame = Some(frame);
            if had_visible || has_visible {
                out.domain.overlay_changed = true;
            }
            (reports, out)
        }
    }
}

pub fn timeline_ranges_view(window: &WindowSession) -> Option<(&[HeatRange], &[SkipRange])> {
    window.timeline_ranges()
}
