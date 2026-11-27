//! Atomic active [`StreamSession`] registration, generation validation, and seek
//! epoch coordination inside [`super::WindowSession`].
use super::{PollReport, WindowSession};
use crate::{
    hub::Binding,
    stream::{prepare::Playback, session::StreamSession},
};
use iris_api::v2::{ErrorCode, Generation, HostEvent, Id};
use iris_core::player::ScopedPlayback;
use std::sync::{Arc, PoisonError};

#[derive(Clone)]
pub(super) struct ActiveStream {
    pub plugin_id: String,
    pub owner: String,
    pub admitted_generation: Generation,
    pub binding: Arc<Binding>,
    pub session: Arc<StreamSession>,
}

impl WindowSession {
    pub(super) fn live_active_stream(&self) -> Option<ActiveStream> {
        let current = self.generation();
        let mut slot = self
            .active_stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let valid = slot.as_ref().is_some_and(|active| {
            active.admitted_generation == current
                && active.binding.is_live()
                && !active.session.cancellation().is_cancelled()
        });
        if !valid {
            *slot = None;
            return None;
        }
        slot.clone()
    }

    /// Atomically registers `playback`'s [`StreamSession`] + `generation` while running
    /// `open`, rolling back and cancelling immediately if `open` fails or `generation` is stale.
    pub fn admit_playback<R, E>(
        &self,
        plugin_id: &str,
        generation: Generation,
        mut playback: Playback,
        open: impl FnOnce(Arc<ScopedPlayback>) -> Result<R, E>,
    ) -> Result<Result<R, E>, ErrorCode> {
        let _notice_lease = playback.notice_lease.take();
        if !self.is_connected() {
            playback.relay.stop();
            return Err(ErrorCode::Cancelled);
        }
        if self.generation() != generation {
            playback.relay.stop();
            return Err(ErrorCode::StaleHandle);
        }
        let Some(binding) = self.binding(plugin_id).cloned() else {
            playback.relay.stop();
            return Err(ErrorCode::StaleHandle);
        };
        let session = playback.relay.session().clone();
        if session.cancellation().is_cancelled() {
            playback.relay.stop();
            return Err(ErrorCode::Cancelled);
        }
        let owner = session.factory.owner().to_owned();
        if binding.installed.installation_id != owner {
            playback.relay.stop();
            return Err(ErrorCode::Denied);
        }
        let scoped = Arc::new(playback).into_scoped();
        {
            let mut slot = self
                .active_stream
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(prev) = slot.replace(ActiveStream {
                plugin_id: plugin_id.to_owned(),
                owner,
                admitted_generation: generation,
                binding,
                session: session.clone(),
            }) && !Arc::ptr_eq(&prev.session, &session)
            {
                prev.session.cancel();
            }
        }
        let result = open(scoped);
        if result.is_err() {
            self.clear_active_stream();
            session.cancel();
        }
        Ok(result)
    }

    /// Clears any registered active [`StreamSession`] (e.g. on playback stop or non-plugin open).
    pub fn clear_active_stream(&self) {
        if let Some(active) = self
            .active_stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            active.session.cancel();
        }
    }

    pub(super) fn clear_active_stream_for_plugin(&self, plugin_id: &str) {
        let removed = {
            let mut slot = self
                .active_stream
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if slot.as_ref().is_some_and(|s| s.plugin_id == plugin_id) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(active) = removed {
            active.session.cancel();
        }
    }

    /// Returns whether a live plugin stream is currently admitted (optionally matching `plugin_id`).
    pub fn has_active_stream_for(&self, plugin_id: Option<&str>) -> bool {
        let Some(active) = self.live_active_stream() else {
            return false;
        };
        match plugin_id {
            None => true,
            Some(target) => {
                active.plugin_id == target
                    || self
                        .list()
                        .iter()
                        .find(|p| p.id == target)
                        .and_then(|p| p.binding.as_ref())
                        .is_some_and(|b| b.installed.installation_id == active.owner)
            }
        }
    }

    /// Sends `HostEvent::StreamStatus` for the currently admitted stream, if live at this generation.
    pub fn notify_stream_status(&self, plugin_id: &str, request_id: Option<Id>) -> PollReport {
        let Some(active) = self.live_active_stream() else {
            return PollReport::default();
        };
        let generation = active.session.generation();
        self.send_to(
            plugin_id,
            HostEvent::StreamStatus {
                request_id,
                session: active.session.id,
                generation,
                manifest_revision: active.session.revision(),
                context_version: generation.context,
            },
        )
    }

    /// Advances the inner timed-overlay seek epoch and notifies the active stream owner of seek status.
    pub fn advance_overlay_seek(&mut self) -> Option<PollReport> {
        let seek = self.enhance.overlay_generation.seek.0.checked_add(1)?;
        self.enhance.overlay_generation.seek = Id(seek);
        self.enhance.reset_overlays();
        self.enhance
            .pending
            .retain(|(_, key), _| !key.starts_with("overlay:"));
        let active = self.live_active_stream()?;
        Some(self.notify_stream_status(&active.plugin_id, None))
    }

    pub fn overlay_generation(&self) -> Generation {
        self.enhance.overlay_generation
    }
}
