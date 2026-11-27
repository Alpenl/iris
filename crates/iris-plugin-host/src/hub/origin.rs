use super::{
    Binding, Notice,
    jobs::{Work, guarded},
    worker::Worker,
};
use crate::network::{
    NetworkGrant,
    consent::{OriginReview, OriginStore, SavedOrigin},
    parse_consent_origin_url,
};
use iris_api::v2::{ErrorCode, Generation, HostEvent, Id, task_result::OriginResult};
use iris_base::net::http_client::HttpCancel;
use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

pub struct OriginSummary {
    pub id: Id,
    pub binding: Arc<Binding>,
    pub generation: Generation,
    pub origin: String,
    pub ips: Vec<IpAddr>,
    pub private: bool,
    pub plaintext: bool,
    pub previous: Option<SavedOrigin>,
}
pub(super) struct PendingOrigin {
    binding: Arc<Binding>,
    window: Id,
    generation: Generation,
    request: Id,
    origin: String,
    network: NetworkGrant,
    store: OriginStore,
    cancel: HttpCancel,
    review: Option<OriginReview>,
    working: bool,
    pub expires: Instant,
}
impl Worker {
    pub(crate) fn request_origin(
        &mut self,
        window: Id,
        binding: Arc<Binding>,
        generation: Generation,
        request: Id,
        raw: &str,
    ) -> Result<(), ErrorCode> {
        if self.origin_slots.len() >= 2
            || self
                .origins
                .values()
                .any(|r| r.binding.installed.installation_id == binding.installed.installation_id)
        {
            return Err(ErrorCode::Backpressure);
        }
        if self
            .active_window(window)
            .is_none_or(|w| w.generation != generation)
            || !binding.is_live()
        {
            return Err(ErrorCode::StaleHandle);
        }
        let (origin, _) = parse_consent_origin_url(raw)?;
        let origin = origin.display();
        let control = self.plugins[&binding.installed.plugin_id]
            .control
            .as_ref()
            .ok_or(ErrorCode::StaleHandle)?;
        let network = control.network().clone();
        let store = control.origins().clone();
        let id = Id(super::super::instance::next_id()?);
        let cancel = HttpCancel::default();
        let (job_network, job_store, job_cancel, job_origin) = (
            network.clone(),
            store.clone(),
            cancel.clone(),
            origin.clone(),
        );
        self.submit(false, "origin-review", move || Work::OriginReviewed {
            id,
            result: guarded(|| job_store.review(&job_network, &job_origin, &job_cancel))
                .map(Box::new),
        })?;
        self.origins.insert(
            id,
            PendingOrigin {
                binding,
                window,
                generation,
                request,
                origin,
                network,
                store,
                cancel,
                review: None,
                working: true,
                expires: Instant::now() + Duration::from_secs(300),
            },
        );
        self.origin_slots.insert(id);
        Ok(())
    }
    pub(crate) fn origin_reviewed(&mut self, id: Id, result: Result<OriginReview, ErrorCode>) {
        let active = self
            .window
            .as_ref()
            .map(|w| (w.inbox.id, w.generation, w.inbox.clone()));
        let Some(pending) = self.origins.get_mut(&id) else {
            self.origin_slots.remove(&id);
            return;
        };
        pending.working = false;
        let result = result.and_then(|review| {
            let (_, win_gen, inbox) = active
                .filter(|(win_id, _, _)| *win_id == pending.window)
                .ok_or(ErrorCode::StaleHandle)?;
            if !pending.binding.is_live() || win_gen != pending.generation {
                return Err(ErrorCode::StaleHandle);
            }
            let summary = Arc::new(OriginSummary {
                id,
                binding: pending.binding.clone(),
                generation: pending.generation,
                origin: review.origin().into(),
                ips: review.ips().to_vec(),
                private: review.private(),
                plaintext: review.plaintext(),
                previous: review.previous().cloned(),
            });
            inbox.push(Notice::Origin(summary))?;
            pending.review = Some(review);
            Ok(())
        });
        if let Err(error) = result {
            self.origin_applied(id, Err(error));
        }
    }
    pub(crate) fn confirm_origin(
        &mut self,
        window: Id,
        id: Id,
        allow: bool,
        insecure: bool,
    ) -> Result<(), ErrorCode> {
        let pending = self.origins.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if pending.window != window {
            return Err(ErrorCode::Denied);
        }
        if self
            .active_window(window)
            .is_none_or(|w| w.generation != pending.generation)
            || pending.expires <= Instant::now()
            || !pending.binding.is_live()
        {
            return Err(ErrorCode::StaleHandle);
        }
        if pending.working || pending.review.is_none() {
            return Err(ErrorCode::Backpressure);
        }
        if !allow {
            self.origin_applied(id, Err(ErrorCode::Denied));
            return Ok(());
        }
        let lease = self.reserve_job_lease()?;
        let pending = self.origins.get_mut(&id).unwrap();
        let review = pending.review.take().unwrap();
        let (network, store, cancel) = (
            pending.network.clone(),
            pending.store.clone(),
            pending.cancel.clone(),
        );
        pending.working = true;
        let result = self.submit_reserved(false, "origin-approve", lease, move || {
            Work::OriginApplied {
                id,
                result: guarded(|| store.approve(&network, review, insecure, &cancel)),
            }
        });
        if let Err(error) = result {
            self.origin_applied(id, Err(error));
        }
        Ok(())
    }
    pub(crate) fn origin_applied(&mut self, id: Id, result: Result<OriginResult, ErrorCode>) {
        self.origin_slots.remove(&id);
        let Some(pending) = self.origins.remove(&id) else {
            return;
        };
        self.deliver_origin_result(id, pending, result);
    }
    fn deliver_origin_result(
        &mut self,
        id: Id,
        pending: PendingOrigin,
        result: Result<OriginResult, ErrorCode>,
    ) {
        pending.cancel.cancel();
        let Some((win_gen, inbox)) = self
            .active_window(pending.window)
            .map(|w| (w.generation, w.inbox.clone()))
        else {
            return;
        };
        let _ = inbox.push(Notice::OriginClosed(id));
        if win_gen != pending.generation || !pending.binding.is_live() {
            return;
        }
        let plugin = &pending.binding.installed.plugin_id;
        let (grant, insecure_auth) = match result {
            Ok(result) => (Some(result.origin_grant), result.insecure_auth),
            Err(error) => {
                if error != ErrorCode::Denied {
                    self.error(
                        pending.window,
                        Some(plugin.clone()),
                        Some(pending.request),
                        error,
                        None,
                    );
                }
                (None, false)
            }
        };
        if let Some(control) = self.plugins.get(plugin).and_then(|p| p.control.as_ref()) {
            let result = control.send(
                pending.window,
                HostEvent::OriginGranted {
                    request_id: pending.request,
                    origin: pending.origin,
                    grant,
                    insecure_auth,
                },
            );
            if let Err(error) = result {
                self.fail_plugin(plugin, error);
            }
        }
    }
    pub(crate) fn expire_origins(&mut self) {
        let active = self.window.as_ref().map(|w| (w.inbox.id, w.generation));
        let expired: Vec<_> = self
            .origins
            .iter()
            .filter_map(|(id, r)| {
                (!r.binding.is_live()
                    || r.expires <= Instant::now()
                    || active != Some((r.window, r.generation)))
                .then_some(*id)
            })
            .collect();
        for id in expired {
            if let Some(pending) = self.origins.remove(&id) {
                pending.cancel.cancel();
                if !pending.working {
                    self.origin_slots.remove(&id);
                }
                self.deliver_origin_result(id, pending, Err(ErrorCode::StaleHandle));
            }
        }
    }
}
