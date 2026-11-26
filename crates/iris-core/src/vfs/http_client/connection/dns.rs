//! System DNS lookup deduplication, bounded worker slots, and caller-linked DoH fallback.

use std::collections::BTreeMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::super::CONNECT_TIMEOUT;
use super::{HttpCancel, NetworkScope, doh, remaining_timeout};

type LookupResult = std::result::Result<Vec<IpAddr>, String>;

struct LookupState {
    result: Option<LookupResult>,
    waiters: usize,
    abandoned: bool,
    deadline: Instant,
}

struct Lookup {
    state: Mutex<LookupState>,
    ready: Condvar,
    worker_cancel: HttpCancel,
}

impl Lookup {
    fn current_deadline(&self) -> Instant {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deadline
    }

    fn try_join(&self, end: Instant) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.result.is_none() && !s.abandoned && !self.worker_cancel.is_cancelled() {
            s.waiters += 1;
            s.deadline = s.deadline.max(end);
            true
        } else {
            false
        }
    }
}

struct DnsThreadGuard;
static ACTIVE_DNS_THREADS: AtomicUsize = AtomicUsize::new(0);
impl Drop for DnsThreadGuard {
    fn drop(&mut self) {
        ACTIVE_DNS_THREADS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct LookupWaiter(Arc<Lookup>);
impl Drop for LookupWaiter {
    fn drop(&mut self) {
        let abandon = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.waiters = state.waiters.saturating_sub(1);
            if state.waiters == 0 && state.result.is_none() {
                state.abandoned = true;
                true
            } else {
                false
            }
        };
        if abandon {
            self.0.worker_cancel.cancel();
        }
    }
}

pub(super) fn resolve_system(
    host: &str,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
    scope: &NetworkScope,
) -> Result<Vec<IpAddr>> {
    // The platform resolver preserves the hosts file, split DNS, and Bonjour.
    // Share one active lookup per host, prune abandoned stalled lookups so
    // hung OS getaddrinfo calls cannot wedge the 16-slot active cap, and link
    // DoH fallback cancellation/deadlines to waiting callers.
    const MAX_ACTIVE_LOOKUPS: usize = 16;
    const MAX_DNS_THREADS: usize = 64;
    static LOOKUPS: Mutex<BTreeMap<String, Arc<Lookup>>> = Mutex::new(BTreeMap::new());

    let end = Instant::now() + remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?;
    let mut pending = LOOKUPS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    pending.retain(|_, entry| {
        let s = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        s.result.is_none()
            && (s.waiters > 0
                || (!s.abandoned && !entry.worker_cancel.is_cancelled() && now < s.deadline))
    });

    let lookup = if let Some(existing) = pending.get(host).cloned() {
        if existing.try_join(end) {
            existing
        } else {
            pending.remove(host);
            spawn_dns_lookup(
                &mut pending,
                host,
                end,
                scope,
                MAX_ACTIVE_LOOKUPS,
                MAX_DNS_THREADS,
                &LOOKUPS,
            )?
        }
    } else {
        spawn_dns_lookup(
            &mut pending,
            host,
            end,
            scope,
            MAX_ACTIVE_LOOKUPS,
            MAX_DNS_THREADS,
            &LOOKUPS,
        )?
    };
    drop(pending);

    let _waiter = LookupWaiter(lookup.clone());
    let wake_lookup = lookup.clone();
    let _cancel_reg = cancel.register_callback(Arc::new(move || {
        let _guard = wake_lookup.state.lock().unwrap_or_else(|e| e.into_inner());
        wake_lookup.ready.notify_all();
    }))?;

    let mut state = lookup.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(result) = state.result.as_ref() {
            return result
                .clone()
                .map_err(|err| anyhow!("system DNS lookup failed: {err}"));
        }
        let left = remaining_timeout(cancel, Some(end), CONNECT_TIMEOUT)?;
        state = lookup
            .ready
            .wait_timeout(state, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn spawn_dns_lookup(
    pending: &mut BTreeMap<String, Arc<Lookup>>,
    host: &str,
    end: Instant,
    scope: &NetworkScope,
    max_active: usize,
    max_threads: usize,
    lookups: &'static Mutex<BTreeMap<String, Arc<Lookup>>>,
) -> Result<Arc<Lookup>> {
    if pending.len() >= max_active {
        bail!("system DNS lookup limit reached");
    }
    ACTIVE_DNS_THREADS
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < max_threads).then_some(n + 1)
        })
        .map_err(|_| anyhow!("system DNS worker thread limit reached"))?;
    let thread_guard = DnsThreadGuard;
    let dns_reservation = scope.policy.as_ref().map(|p| p.reserve_dns()).transpose()?;
    let lookup = Arc::new(Lookup {
        state: Mutex::new(LookupState {
            result: None,
            waiters: 1,
            abandoned: false,
            deadline: end,
        }),
        ready: Condvar::new(),
        worker_cancel: HttpCancel::default(),
    });
    let worker = lookup.clone();
    let hostname = host.to_owned();
    std::thread::Builder::new()
        .name("iris-http-dns".into())
        .spawn(move || {
            let _thread_guard = thread_guard;
            let _dns_reservation = dns_reservation;
            let result = (hostname.as_str(), 0)
                .to_socket_addrs()
                .map_err(|err| err.to_string())
                .and_then(|addrs| {
                    let ips: Vec<_> = addrs.take(65).map(|addr| addr.ip()).collect();
                    if ips.len() > 64 {
                        Err("system DNS result exceeds address limit".into())
                    } else if doh::should_bypass_fake_ip(&hostname, &ips) {
                        doh::resolve_a_records(&hostname, &worker.worker_cancel, || {
                            worker.current_deadline()
                        })
                        .map_err(|err| format!("DoH fallback failed: {err}"))
                    } else {
                        Ok(ips)
                    }
                });
            {
                let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
                state.result = Some(result);
            }
            worker.ready.notify_all();
            let mut map = lookups.lock().unwrap_or_else(|e| e.into_inner());
            if map.get(&hostname).is_some_and(|e| Arc::ptr_eq(e, &worker)) {
                map.remove(&hostname);
            }
        })?;
    pending.insert(host.to_owned(), lookup.clone());
    Ok(lookup)
}

#[cfg(test)]
mod tests {
    use super::{HttpCancel, Lookup, LookupState, LookupWaiter};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn lookup_waiter_cancels_worker_only_when_last_waiter_leaves() {
        let lookup = Arc::new(Lookup {
            state: Mutex::new(LookupState {
                result: None,
                waiters: 2,
                abandoned: false,
                deadline: Instant::now() + Duration::from_secs(5),
            }),
            ready: Condvar::new(),
            worker_cancel: HttpCancel::default(),
        });
        let w1 = LookupWaiter(lookup.clone());
        let w2 = LookupWaiter(lookup.clone());
        drop(w1);
        assert!(!lookup.worker_cancel.is_cancelled());
        drop(w2);
        assert!(lookup.worker_cancel.is_cancelled());
    }

    #[test]
    fn joining_waiter_extends_live_worker_deadline_past_initial_caller_timeout() {
        let now = Instant::now();
        let short = now + Duration::from_millis(20);
        let extended = now + Duration::from_secs(10);
        let lookup = Arc::new(Lookup {
            state: Mutex::new(LookupState {
                result: None,
                waiters: 1,
                abandoned: false,
                deadline: short,
            }),
            ready: Condvar::new(),
            worker_cancel: HttpCancel::default(),
        });
        let w1 = LookupWaiter(lookup.clone());
        assert_eq!(lookup.current_deadline(), short);

        assert!(lookup.try_join(extended));
        let w2 = LookupWaiter(lookup.clone());
        drop(w1);
        assert!(!lookup.worker_cancel.is_cancelled());
        assert_eq!(lookup.current_deadline(), extended);
        drop(w2);
        assert!(lookup.worker_cancel.is_cancelled());
        assert!(!lookup.try_join(extended));
    }

    #[test]
    fn try_join_rejects_lookup_marked_abandoned_before_worker_cancel_completes() {
        let lookup = Arc::new(Lookup {
            state: Mutex::new(LookupState {
                result: None,
                waiters: 0,
                abandoned: true,
                deadline: Instant::now() + Duration::from_secs(10),
            }),
            ready: Condvar::new(),
            worker_cancel: HttpCancel::default(),
        });
        assert!(!lookup.worker_cancel.is_cancelled());
        assert!(!lookup.try_join(Instant::now() + Duration::from_secs(10)));
    }
}
