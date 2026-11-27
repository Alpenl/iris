use super::super::{AppModel, views::DirtyRegions};
use gpui::Context;
use iris_api::v2::{ErrorCode, Id};
use iris_plugin_host::ui::{EnhancementOutcome, EnhancementRequest, ReadyEffect};

impl AppModel {
    pub(in crate::ui) fn apply_plugin_enhancement(
        &mut self,
        request: EnhancementRequest,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        let (media_loaded, position_ms) = {
            let state = self.active().session.state();
            (state.loaded, (state.time_pos.max(0.) * 1000.) as u64)
        };
        let store = &self.store;
        let outcome = self
            .plugins
            .execute_enhancement(request, media_loaded, position_ms, || {
                store.annotation_target()
            });
        match outcome {
            EnhancementOutcome::Queued | EnhancementOutcome::Noop => DirtyRegions {
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            EnhancementOutcome::Immediate(effect) => {
                self.apply_ready_effect(effect, cx).union(DirtyRegions {
                    viewport: true,
                    dock: true,
                    ..DirtyRegions::NONE
                })
            }
            EnhancementOutcome::Rejected {
                code,
                superseded,
                report,
            } => {
                let mut regions = self
                    .apply_poll_report(report, None, cx)
                    .union(DirtyRegions {
                        viewport: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    });
                if superseded {
                    log::debug!("ignoring superseded plugin enhancement action: {code:?}");
                } else {
                    log::warn!("plugin enhancement action failed: {code:?}");
                    self.show_toast(
                        format!(
                            "插件操作未完成 · {}",
                            super::notices::describe_error_code(code)
                        ),
                        cx,
                    );
                    regions.root = true;
                }
                regions
            }
        }
    }

    pub(in crate::ui) fn apply_ready_effect(
        &mut self,
        effect: ReadyEffect,
        cx: &mut Context<Self>,
    ) -> DirtyRegions {
        match effect {
            ReadyEffect::Timeline(lease) => {
                let chapters = lease.chapters();
                let owner = lease.owner().to_owned();
                match self
                    .active_mut()
                    .session
                    .set_plugin_timeline(owner, &chapters)
                {
                    Ok(()) => {
                        self.plugins.commit_timeline(lease);
                        DirtyRegions::dock_only()
                    }
                    Err(_) => {
                        log::warn!(
                            "plugin enhancement action failed: {:?}",
                            ErrorCode::BadState
                        );
                        self.show_toast(
                            format!(
                                "插件操作未完成 · {}",
                                super::notices::describe_error_code(ErrorCode::BadState)
                            ),
                            cx,
                        );
                        DirtyRegions::root_only()
                    }
                }
            }
            ReadyEffect::Attachment(attachment) => {
                let mut regions = DirtyRegions::dock_only();
                if let Err(error) = self
                    .active_mut()
                    .session
                    .attach_plugin_track(attachment.into_external_track())
                {
                    self.show_toast(format!("媒体轨道添加失败 · {error}"), cx);
                    regions.root = true;
                }
                regions
            }
            ReadyEffect::MonitorLut(ready_lut) => {
                let (owner, filter) = ready_lut.into_filter();
                let mut regions = DirtyRegions::viewport_only();
                match self.active_mut().session.set_monitor_lut(Some(filter)) {
                    Ok(()) => self.plugins.commit_lut_owner(owner),
                    Err(error) => {
                        self.show_toast(format!("LUT 未应用 · {error}"), cx);
                        regions.root = true;
                    }
                }
                regions
            }
            ReadyEffect::ClearMonitorLut { owner } => {
                match self.active_mut().session.set_monitor_lut(None) {
                    Ok(()) => {
                        self.plugins.confirm_lut_cleared(&owner);
                        DirtyRegions::viewport_only()
                    }
                    Err(_) => {
                        log::warn!("plugin enhancement action failed: {:?}", ErrorCode::IoError);
                        self.show_toast(
                            format!(
                                "插件操作未完成 · {}",
                                super::notices::describe_error_code(ErrorCode::IoError)
                            ),
                            cx,
                        );
                        DirtyRegions::root_only()
                    }
                }
            }
            ReadyEffect::TimedOverlay(_ready) => {
                self.plugin_view.overlay_wake = None;
                self.step_timed_overlay_frame(cx);
                DirtyRegions::viewport_only()
            }
            ReadyEffect::StreamUpdated(ready) => {
                self.notify_stream_status(&ready.plugin_id, None, cx);
                DirtyRegions::NONE
            }
            ReadyEffect::Annotations(ready) => {
                let ready = *ready;
                if let Some(snapshot) = ready.snapshot {
                    let _ = self.store.apply_annotation_snapshot(snapshot);
                }
                let report = self.plugins.send_to(&ready.plugin_id, ready.event);
                self.apply_poll_report(report, None, cx)
                    .union(DirtyRegions {
                        viewport: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    })
            }
            ReadyEffect::Failed { code, .. } => {
                log::warn!("plugin enhancement job failed: {code:?}");
                self.show_toast(
                    format!(
                        "插件操作未完成 · {}",
                        super::notices::describe_error_code(code)
                    ),
                    cx,
                );
                DirtyRegions::root_only()
            }
        }
    }

    pub(in crate::ui) fn notify_stream_status(
        &mut self,
        plugin: &str,
        request_id: Option<Id>,
        cx: &mut Context<Self>,
    ) {
        let report = self.plugins.notify_stream_status(plugin, request_id);
        let regions = self.apply_poll_report(report, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }
}
