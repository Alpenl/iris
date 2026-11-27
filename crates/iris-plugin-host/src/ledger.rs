//! Reservations cover queued, running and undelivered resources. Cancellation
//! never releases a reservation before its owner actually finishes.
use iris_api::v2::ErrorCode;
use iris_base::rt::budget;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Resource {
    WasmBytes,
    HostBytes,
    ControlStore,
    StreamStore,
    Task,
    Handle,
    Http,
    ControlHttp,
    WebSocket,
    Tcp,
    StreamRead,
    FileHash,
    TempBytes,
    Dns,
    Auth,
    PlaybackSession,
    StreamResource,
    RelayWorker,
    OverlayCues,
    OverlayBytes,
    EnhancementTrack,
}

impl Resource {
    fn limits(self) -> (u64, u64) {
        match self {
            Self::WasmBytes => (512 << 20, 1024 << 20),
            Self::HostBytes => (128 << 20, 512 << 20),
            Self::ControlStore => (
                budget::PLUGIN_CONTROL_STORES_PER_PLUGIN,
                budget::PLUGIN_CONTROL_STORES_GLOBAL,
            ),
            Self::StreamStore => (
                budget::PLUGIN_STREAM_STORES_PER_PLUGIN,
                budget::PLUGIN_STREAM_STORES_GLOBAL,
            ),
            Self::Task => (32, 128),
            Self::Handle => (128, 1024),
            Self::Http => (12, 68),
            Self::ControlHttp => (8, 56),
            Self::WebSocket | Self::Tcp => (
                budget::PLUGIN_SOCKET_PUMPS_PER_KIND_PLUGIN,
                budget::PLUGIN_SOCKET_PUMPS_PER_KIND_GLOBAL,
            ),
            Self::StreamRead => (8, 32),
            Self::FileHash => (1, 2),
            Self::TempBytes => (1 << 30, 4 << 30),
            Self::Dns => (
                budget::PLUGIN_ORIGIN_DNS_PER_PLUGIN,
                budget::PLUGIN_ORIGIN_DNS_GLOBAL,
            ),
            Self::Auth => (
                budget::PLUGIN_AUTH_CALLBACKS_PER_PLUGIN,
                budget::PLUGIN_AUTH_CALLBACKS_GLOBAL,
            ),
            Self::PlaybackSession => (
                budget::PLUGIN_RELAY_LISTENERS_PER_PLUGIN,
                budget::PLUGIN_RELAY_LISTENERS_GLOBAL,
            ),
            Self::StreamResource => (45_000, 180_000),
            Self::RelayWorker => (
                budget::PLUGIN_RELAY_WORKERS_PER_RELAY as u64,
                budget::PLUGIN_RELAY_WORKERS_GLOBAL,
            ),
            Self::OverlayCues => (10_000, 40_000),
            Self::OverlayBytes => (4 << 20, 16 << 20),
            Self::EnhancementTrack => (8, 64),
        }
    }
}

type Counts = BTreeMap<Resource, u64>;

/// Tracks workers that can outlive an accepted task (notably system DNS).
#[derive(Clone, Default)]
pub struct Activity(Arc<AtomicUsize>);
pub struct ActivityLease(Arc<AtomicUsize>);
impl Activity {
    pub fn start(&self) -> ActivityLease {
        self.0.fetch_add(1, Ordering::AcqRel);
        ActivityLease(self.0.clone())
    }
    pub fn is_idle(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}
impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Default)]
struct State {
    global: Counts,
    plugins: BTreeMap<String, Counts>,
}

#[derive(Clone, Default)]
pub struct Ledger(Arc<Mutex<State>>);

/// Non-cloneable ownership token; transfers move the token with the resource.
pub struct Reservation {
    ledger: Ledger,
    owner: String,
    counts: Counts,
}

impl Reservation {
    /// Replace one exclusively owned resident snapshot. The caller separately
    /// reserves scratch memory while constructing the replacement. On rejection
    /// neither the old lease nor any counter changes.
    pub(crate) fn replace(&mut self, amounts: &[(Resource, u64)]) -> Result<(), ErrorCode> {
        let mut counts = Counts::new();
        for &(kind, amount) in amounts {
            let count = counts.entry(kind).or_default();
            *count = count.checked_add(amount).ok_or(ErrorCode::QuotaExceeded)?;
        }
        let mut state = self.ledger.0.lock().unwrap_or_else(|e| e.into_inner());
        for (&kind, &amount) in &counts {
            let old = self.counts.get(&kind).copied().unwrap_or(0);
            let per = state
                .plugins
                .get(&self.owner)
                .and_then(|p| p.get(&kind))
                .copied()
                .unwrap_or(0)
                - old;
            let all = state.global.get(&kind).copied().unwrap_or(0) - old;
            let (max_per, max_all) = kind.limits();
            if amount > max_per.saturating_sub(per) || amount > max_all.saturating_sub(all) {
                log::warn!(
                    "plugin ledger replace quota exceeded: owner={} kind={kind:?} requested={amount} plugin_used={per}/{max_per} global_used={all}/{max_all}",
                    self.owner
                );
                return Err(ErrorCode::QuotaExceeded);
            }
        }
        for (&kind, &old) in &self.counts {
            *state.global.entry(kind).or_default() -= old;
            *state
                .plugins
                .entry(self.owner.clone())
                .or_default()
                .entry(kind)
                .or_default() -= old;
        }
        for (&kind, &amount) in &counts {
            *state.global.entry(kind).or_default() += amount;
            *state
                .plugins
                .entry(self.owner.clone())
                .or_default()
                .entry(kind)
                .or_default() += amount;
        }
        self.counts = counts;
        Ok(())
    }
    /// Atomically grow an existing lease before allocating/writing more data.
    pub fn extend(&mut self, amounts: &[(Resource, u64)]) -> Result<(), ErrorCode> {
        let mut extra = self.ledger.reserve(&self.owner, amounts)?;
        for (kind, amount) in std::mem::take(&mut extra.counts) {
            *self.counts.entry(kind).or_default() += amount;
        }
        Ok(())
    }
}

impl Ledger {
    pub fn reserve(
        &self,
        owner: &str,
        amounts: &[(Resource, u64)],
    ) -> Result<Reservation, ErrorCode> {
        let mut counts = Counts::new();
        for &(kind, amount) in amounts {
            let n = counts.entry(kind).or_default();
            *n = n.checked_add(amount).ok_or(ErrorCode::QuotaExceeded)?;
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (&kind, &amount) in &counts {
            let (per_plugin, global) = kind.limits();
            let plugin_used = state
                .plugins
                .get(owner)
                .and_then(|p| p.get(&kind))
                .copied()
                .unwrap_or(0);
            let global_used = state.global.get(&kind).copied().unwrap_or(0);
            if amount > per_plugin.saturating_sub(plugin_used)
                || amount > global.saturating_sub(global_used)
            {
                log::warn!(
                    "plugin ledger reserve quota exceeded: owner={owner} kind={kind:?} requested={amount} plugin_used={plugin_used}/{per_plugin} global_used={global_used}/{global}"
                );
                return Err(ErrorCode::QuotaExceeded);
            }
        }
        for (&kind, &amount) in &counts {
            *state.global.entry(kind).or_default() += amount;
            *state
                .plugins
                .entry(owner.to_owned())
                .or_default()
                .entry(kind)
                .or_default() += amount;
        }
        Ok(Reservation {
            ledger: self.clone(),
            owner: owner.to_owned(),
            counts,
        })
    }

    pub fn used(&self, owner: Option<&str>, kind: Resource) -> u64 {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match owner {
            Some(owner) => state
                .plugins
                .get(owner)
                .and_then(|p| p.get(&kind))
                .copied()
                .unwrap_or(0),
            None => state.global.get(&kind).copied().unwrap_or(0),
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.ledger.0.lock().unwrap_or_else(|e| e.into_inner());
        for (&kind, &amount) in &self.counts {
            if let Some(total) = state.global.get_mut(&kind) {
                *total -= amount;
            }
            if let Some(total) = state
                .plugins
                .get_mut(&self.owner)
                .and_then(|p| p.get_mut(&kind))
            {
                *total -= amount;
            }
        }
        if state
            .plugins
            .get(&self.owner)
            .is_some_and(|p| p.values().all(|&n| n == 0))
        {
            state.plugins.remove(&self.owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_are_atomic_across_instances_and_return_on_drop() {
        let ledger = Ledger::default();
        let hold = ledger.reserve("p", &[(Resource::Handle, 128)]).unwrap();
        assert!(
            ledger
                .reserve("p", &[(Resource::Task, 1), (Resource::Handle, 1)])
                .is_err()
        );
        assert_eq!(ledger.used(None, Resource::Task), 0);
        assert!(ledger.reserve("other", &[(Resource::Handle, 1)]).is_ok());
        drop(hold);
        assert_eq!(ledger.used(None, Resource::Handle), 0);
    }
    #[test]
    fn control_http_cannot_consume_reserved_stream_capacity() {
        let ledger = Ledger::default();
        let holds: Vec<_> = (0..7)
            .map(|n| {
                ledger
                    .reserve(
                        &n.to_string(),
                        &[(Resource::Http, 8), (Resource::ControlHttp, 8)],
                    )
                    .unwrap()
            })
            .collect();
        assert!(
            ledger
                .reserve("next", &[(Resource::Http, 1), (Resource::ControlHttp, 1)])
                .is_err()
        );
        assert!(ledger.reserve("stream", &[(Resource::Http, 8)]).is_ok());
        drop(holds);
        assert_eq!(ledger.used(None, Resource::Http), 0);
    }
}
