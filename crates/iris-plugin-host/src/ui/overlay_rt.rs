//! Timed-overlay clock/window/frame coordination, enhancement lifetime reconciliation,
//! and bounded remote thumbnail workers for [`super::WindowSession`].
use super::{
    PollReport, WindowSession,
    effects::{
        OverlayStep, REMOTE_THUMB_LEASE_BYTES, REMOTE_THUMB_SCRATCH_BYTES, RetainedThumbnail,
        RevokedEnhancements,
    },
};
use crate::{
    hub::Binding,
    ledger::Resource,
    overlay::OverlayBank,
    runtime::{BackgroundResult, WindowAccess},
    stream::remote::RemoteFactory,
};
use futures::channel::oneshot;
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostEvent, enhance::OverlayWindow, stream::RemoteResource,
};
use iris_base::net::http_client::HttpCancel;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Completion channel for [`WindowSession::spawn_thumbnail_fetch`].
pub type ThumbnailReceiver<T> =
    oneshot::Receiver<Result<BackgroundResult<RetainedThumbnail<T>>, ErrorCode>>;

impl WindowSession {
    pub(super) fn ensure_overlay_bank(
        &mut self,
        access: &WindowAccess,
        position_ms: u64,
    ) -> Arc<Mutex<OverlayBank>> {
        let overlay_gen = self.enhance.overlay_generation;
        self.enhance
            .bank
            .get_or_insert_with(|| {
                Arc::new(Mutex::new(OverlayBank::new(
                    access.resources.ledger.clone(),
                    overlay_gen.media,
                    overlay_gen,
                    position_ms,
                )))
            })
            .clone()
    }

    pub fn reset_timed_overlays(&mut self) {
        self.enhance.reset_overlays();
    }

    pub fn update_overlay_clock(
        &mut self,
        position_secs: f64,
        speed: f64,
        paused: bool,
        buffering: bool,
        duration: f64,
        now: Instant,
    ) {
        self.enhance
            .clock
            .update(position_secs, speed, paused, buffering, duration, now);
    }

    /// Reconciles dead enhancement `Binding`s and initializes `OverlayBank` for ready
    /// `UiOverlay` plugins.
    pub fn reconcile_enhancements(
        &mut self,
        media_loaded: bool,
        position_ms: u64,
        now: Instant,
    ) -> (RevokedEnhancements, Vec<PollReport>) {
        let dead: Vec<_> = self
            .enhance
            .bindings
            .iter()
            .filter(|(_, binding)| !binding.is_live())
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = RevokedEnhancements::default();
        for owner in dead {
            self.enhance.selections.remove(&owner);
            if self
                .enhance
                .timeline
                .as_ref()
                .is_some_and(|lease| lease.owner() == owner)
            {
                self.enhance.timeline = None;
                out.clear_timeline = true;
            }
            if self.enhance.lut_owner.as_deref() == Some(&owner) {
                self.enhance.lut_owner = None;
                out.clear_lut = true;
            }
            self.enhance.pending.retain(|(id, _), _| *id != owner);
            if let Some(bank) = &self.enhance.bank {
                let Ok(mut guard) = bank.try_lock() else {
                    continue;
                };
                guard.revoke(&owner);
            }
            self.enhance.bindings.remove(&owner);
            out.revoked_owners.push(owner);
        }
        let overlays: Vec<_> = self
            .list()
            .iter()
            .filter_map(|p| {
                p.binding
                    .as_ref()
                    .filter(|b| {
                        b.is_live()
                            && b.installed.grants.contains(&Capability::UiOverlay)
                            && self.client.is_ready(&p.id)
                    })
                    .cloned()
            })
            .collect();
        for binding in overlays {
            let owner = binding.installed.installation_id.clone();
            let Ok(access) = self.client.access(&binding.installed.plugin_id) else {
                continue;
            };
            self.ensure_overlay_bank(&access, position_ms);
            self.enhance.bindings.insert(owner, binding);
        }
        let reports = self.update_overlay_window(media_loaded, now);
        (out, reports)
    }

    pub fn update_overlay_window(&mut self, media_loaded: bool, now: Instant) -> Vec<PollReport> {
        if !media_loaded {
            return Vec::new();
        }
        let Some(bank) = self.enhance.bank.as_ref() else {
            return Vec::new();
        };
        let position = self.enhance.clock.position_ms(now);
        let (changed, window) = {
            let Ok(mut guard) = bank.try_lock() else {
                return Vec::new();
            };
            let changed = guard
                .advance(
                    self.generation().media,
                    self.enhance.overlay_generation,
                    position,
                )
                .unwrap_or(false);
            (changed, guard.window().clone())
        };
        self.collect_overlay_window_reports(changed, window)
    }

    pub fn step_overlay_frame(&mut self, media_loaded: bool, now: Instant) -> OverlayStep {
        if !media_loaded {
            return OverlayStep::Empty;
        }
        let Some(bank) = self.enhance.bank.clone() else {
            return OverlayStep::Empty;
        };
        let clock = &self.enhance.clock;
        let (position, paused, speed) = (
            clock.position_ms(now),
            clock.effectively_paused(now),
            clock.speed(),
        );
        let generation = self.enhance.overlay_generation;
        let has_unannounced = self.enhance.bindings.values().any(|b| {
            b.is_live()
                && b.installed.grants.contains(&Capability::UiOverlay)
                && self.enhance.announced.get(&b.installed.installation_id) != Some(&generation)
        });
        let (frame, window_changed) = {
            let Ok(mut guard) = bank.try_lock() else {
                return OverlayStep::Busy {
                    retry_after: (!paused).then_some(Duration::from_millis(16)),
                };
            };
            let changed = guard
                .advance(self.generation().media, generation, position)
                .unwrap_or(false);
            let window_changed =
                (changed || has_unannounced).then(|| (changed, guard.window().clone()));
            let frame = (!guard.is_empty()).then(|| guard.frame(position, paused, speed));
            (frame, window_changed)
        };
        let reports = window_changed
            .map(|(changed, next_window)| self.collect_overlay_window_reports(changed, next_window))
            .unwrap_or_default();
        let Some(Ok(mut frame)) = frame else {
            return OverlayStep::Updated {
                frame: None,
                reports,
            };
        };
        frame.cues.retain(|cue| {
            self.enhance
                .bindings
                .get(&cue.owner)
                .is_some_and(|b| b.is_live())
        });
        OverlayStep::Updated {
            frame: Some(Arc::new(frame)),
            reports,
        }
    }

    fn collect_overlay_window_reports(
        &mut self,
        changed: bool,
        window: OverlayWindow,
    ) -> Vec<PollReport> {
        let generation = self.enhance.overlay_generation;
        let media = self.generation().media;
        let recipients: Vec<_> = self
            .enhance
            .bindings
            .values()
            .filter(|b| {
                b.is_live()
                    && b.installed.grants.contains(&Capability::UiOverlay)
                    && (changed
                        || self.enhance.announced.get(&b.installed.installation_id)
                            != Some(&generation))
            })
            .map(|b| {
                (
                    b.installed.plugin_id.clone(),
                    b.installed.installation_id.clone(),
                )
            })
            .collect();
        let mut reports = Vec::new();
        for (plugin, owner) in recipients {
            let report = self.send_to(
                &plugin,
                HostEvent::OverlayWindowChanged {
                    media,
                    generation,
                    window: window.clone(),
                },
            );
            if report.notices.is_empty() {
                self.enhance.announced.insert(owner, generation);
            }
            reports.push(report);
        }
        reports
    }

    /// Schedules a bounded background remote thumbnail fetch + decode with `Ledger`
    /// scratch and cache reservations. Returns `Ok(None)` when the plugin is not
    /// currently attached/eligible without triggering quota cooldowns.
    pub fn spawn_thumbnail_fetch<T: Send + 'static>(
        &self,
        plugin_id: &str,
        binding: Arc<Binding>,
        resource: RemoteResource,
        parent_cancel: &HttpCancel,
        decode: impl FnOnce(&[u8]) -> Option<T> + Send + 'static,
    ) -> Result<Option<(Generation, ThumbnailReceiver<T>)>, ErrorCode> {
        let Ok(access) = self.client.access(plugin_id) else {
            return Ok(None);
        };
        let generation = access.generation;
        if access.check(generation, Capability::MediaSource).is_err() {
            return Ok(None);
        }
        let Ok(cancel) = access.request_cancel(parent_cancel) else {
            return Ok(None);
        };
        let rx = access.background(move |access| {
            if !binding.is_live() {
                return Err(ErrorCode::Cancelled);
            }
            let scratch = access.resources.ledger.reserve(
                &access.owner,
                &[(Resource::HostBytes, REMOTE_THUMB_SCRATCH_BYTES)],
            )?;
            let lease = access.resources.ledger.reserve(
                &access.owner,
                &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)],
            )?;
            let fetched =
                RemoteFactory::new(access.network.clone(), access.resources.ledger.clone())?
                    .fetch_thumbnail(&resource, &cancel);
            let image = fetched.ok().and_then(|data| decode(data.bytes()));
            drop(scratch);
            let retained = RetainedThumbnail::new(image, lease);
            if cancel.is_cancelled() || !binding.is_live() {
                return Err(ErrorCode::Cancelled);
            }
            Ok(retained)
        })?;
        Ok(Some((generation, rx)))
    }
}
