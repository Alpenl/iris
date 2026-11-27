//! Unified cancellation tokens, monotonic deadlines, and the workspace-wide OS
//! thread inventory and budget registry.
//!
//! All production OS threads across `iris-core` and `iris` must be spawned via
//! [`spawn_thread`] or [`spawn_thread_named`] with a registered [`ThreadRole`].
//! `scripts/check-project.py` forbids raw `std::thread::spawn` and unregistered
//! `std::thread::Builder` calls in production code and verifies every role's
//! documented name in `docs/ARCHITECTURE.md`.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mio::Waker;

/// Centralized concurrency and OS thread caps shared across network, playback,
/// plugin runtime, and application services.
pub mod budget {
    pub const CONTROL_PIPE_THREADS: usize = 1;
    pub const SINGLE_INSTANCE_THREADS: usize = 1;
    pub const VIDEO_SURFACE_THREADS_PER_VIEWPORT: usize = 1;
    pub const MPV_EVENT_PUMP_THREADS_PER_SESSION: usize = 1;

    pub const MEDIA_RELAY_LISTENERS_GLOBAL: usize = 8;
    pub const MEDIA_RELAY_REAPERS_GLOBAL: usize = 8;
    pub const MEDIA_RELAY_WORKERS_PER_RELAY: usize = 8;
    pub const MEDIA_RELAY_WORKERS_GLOBAL: usize = 32;

    pub const SYSTEM_DNS_ACTIVE_LOOKUPS: usize = 16;
    pub const SYSTEM_DNS_WORKERS_GLOBAL: usize = 64;

    pub const WASM_EPOCH_THREADS_PER_ENGINE: usize = 1;
    pub const ARTIFACT_CLEANUP_THREADS_GLOBAL: usize = 1;
    pub const PLUGIN_COORDINATOR_THREADS_PER_HUB: usize = 1;

    pub const PLUGIN_CONTROL_STORES_PER_PLUGIN: u64 = 1;
    pub const PLUGIN_CONTROL_STORES_GLOBAL: u64 = 64;
    pub const PLUGIN_STREAM_STORES_PER_PLUGIN: u64 = 2;
    pub const PLUGIN_STREAM_STORES_GLOBAL: u64 = 8;

    pub const PLUGIN_CONTROL_IO_WORKERS: usize = 4;
    pub const PLUGIN_STREAM_IO_WORKERS: usize = 8;
    pub const PLUGIN_PACKAGE_IO_WORKERS: usize = 2;
    pub const PLUGIN_SETUP_IO_WORKERS: usize = 2;

    pub const PLUGIN_AUTH_CALLBACKS_PER_PLUGIN: u64 = 1;
    pub const PLUGIN_AUTH_CALLBACKS_GLOBAL: u64 = 4;
    pub const PLUGIN_ORIGIN_DNS_PER_PLUGIN: u64 = 6;
    pub const PLUGIN_ORIGIN_DNS_GLOBAL: u64 = 16;
    pub const PLUGIN_SOCKET_PUMPS_PER_KIND_PLUGIN: u64 = 4;
    pub const PLUGIN_SOCKET_PUMPS_PER_KIND_GLOBAL: u64 = 16;
    pub const PLUGIN_RELAY_LISTENERS_PER_PLUGIN: u64 = 4;
    pub const PLUGIN_RELAY_LISTENERS_GLOBAL: u64 = 12;
    pub const PLUGIN_RELAY_WORKERS_PER_RELAY: usize = 8;
    pub const PLUGIN_RELAY_WORKERS_GLOBAL: u64 = 32;
}

/// Reason a cancellable or deadline-bounded wait terminated early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    Cancelled,
    TimedOut,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::TimedOut => f.write_str("operation deadline exceeded"),
        }
    }
}

impl std::error::Error for WaitError {}

/// Monotonic wall-clock deadline shared across blocking I/O, condition-variable
/// waits, `mio::Poll` loops, and Wasm host call checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(Instant);

impl Deadline {
    pub const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    pub fn from_optional(instant: Option<Instant>) -> Option<Self> {
        instant.map(Self)
    }

    pub const fn instant(self) -> Instant {
        self.0
    }

    pub fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }

    pub fn min_duration(self, cap: Duration) -> Self {
        self.min(Self::after(cap))
    }

    pub fn is_expired(self) -> bool {
        Instant::now() >= self.0
    }

    pub fn remaining(self) -> std::result::Result<Duration, WaitError> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(WaitError::TimedOut)
    }

    pub fn remaining_capped(self, cap: Duration) -> std::result::Result<Duration, WaitError> {
        self.remaining().map(|d| d.min(cap))
    }
}

/// Hierarchical, lock-free-polled cancellation token that interrupts blocked
/// TCP sockets, `mio::Poll` loops, `Condvar` waiters, and child scopes.
#[derive(Default, Clone)]
pub struct CancelToken(Arc<CancelInner>);

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    state: Mutex<CancelState>,
}

#[derive(Default)]
struct CancelState {
    next_id: u64,
    sockets: BTreeMap<u64, TcpStream>,
    connects: BTreeMap<u64, Arc<Waker>>,
    callbacks: BTreeMap<u64, Arc<dyn Fn() + Send + Sync>>,
    children: Vec<Weak<CancelInner>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock-free check suitable for tight I/O loops and Wasm host-import gates.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Link `child` so cancelling `self` also cancels `child` without transferring ownership.
    pub fn link(&self, child: &Self) -> Result<()> {
        if Arc::ptr_eq(&self.0, &child.0) {
            return Ok(());
        }
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            drop(state);
            child.cancel();
            bail!(WaitError::Cancelled);
        }
        state.children.retain(|w| w.strong_count() > 0);
        let weak = Arc::downgrade(&child.0);
        if !state.children.iter().any(|w| w.ptr_eq(&weak)) {
            state.children.push(weak);
        }
        Ok(())
    }

    /// Transitions the token to cancelled, shutting down registered sockets and
    /// running wakeups outside the internal lock. Returns `true` on first cancel.
    pub fn cancel(&self) -> bool {
        let (callbacks, children) = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            if self.0.cancelled.swap(true, Ordering::AcqRel) {
                return false;
            }
            for socket in state.sockets.values() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            for wake in state.connects.values() {
                let _ = wake.wake();
            }
            (
                std::mem::take(&mut state.callbacks),
                std::mem::take(&mut state.children),
            )
        };
        for wake in callbacks.into_values() {
            wake();
        }
        for child in children {
            if let Some(arc) = child.upgrade() {
                Self(arc).cancel();
            }
        }
        true
    }

    pub fn child(&self) -> Result<Self> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            bail!(WaitError::Cancelled);
        }
        state.children.retain(|w| w.strong_count() > 0);
        let child = Self::new();
        state.children.push(Arc::downgrade(&child.0));
        Ok(child)
    }

    /// Checks cancellation and an optional [`Deadline`] in one step.
    pub fn check(
        &self,
        deadline: Option<Deadline>,
    ) -> std::result::Result<Option<Duration>, WaitError> {
        if self.is_cancelled() {
            return Err(WaitError::Cancelled);
        }
        deadline.map(Deadline::remaining).transpose()
    }

    /// Computes the remaining timeout bounded by `limit`, failing immediately on
    /// cancellation or an expired [`Deadline`].
    pub fn remaining_timeout(
        &self,
        deadline: Option<Deadline>,
        limit: Duration,
    ) -> std::result::Result<Duration, WaitError> {
        if self.is_cancelled() {
            return Err(WaitError::Cancelled);
        }
        match deadline {
            Some(d) => d.remaining_capped(limit),
            None => {
                if limit.is_zero() {
                    Err(WaitError::TimedOut)
                } else {
                    Ok(limit)
                }
            }
        }
    }

    pub fn register(&self, socket: &TcpStream) -> Result<SocketRegistration> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            bail!(WaitError::Cancelled);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.sockets.insert(id, socket.try_clone()?);
        Ok(SocketRegistration(self.clone(), id))
    }

    pub fn register_connect(&self, wake: Waker) -> Result<ConnectRegistration> {
        self.register_shared_wake(Arc::new(wake))
    }

    pub fn register_shared_wake(&self, wake: Arc<Waker>) -> Result<ConnectRegistration> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            bail!(WaitError::Cancelled);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.connects.insert(id, wake);
        Ok(ConnectRegistration(self.clone(), id))
    }

    /// Registers a wakeup callback invoked after `CancelState`'s lock is released
    /// when [`Self::cancel`] runs. Fails immediately if already cancelled.
    pub fn register_callback(
        &self,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<CancelCallbackRegistration> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            bail!(WaitError::Cancelled);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.callbacks.insert(id, wake);
        Ok(CancelCallbackRegistration(self.clone(), id))
    }
}

pub struct SocketRegistration(CancelToken, u64);

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        self.0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sockets
            .remove(&self.1);
    }
}

pub struct ConnectRegistration(CancelToken, u64);

impl Drop for ConnectRegistration {
    fn drop(&mut self) {
        self.0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .connects
            .remove(&self.1);
    }
}

pub struct CancelCallbackRegistration(CancelToken, u64);

impl Drop for CancelCallbackRegistration {
    fn drop(&mut self) {
        self.0
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callbacks
            .remove(&self.1);
    }
}

/// Complete enumeration of OS thread roles permitted in production builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(usize)]
pub enum ThreadRole {
    ControlPipe = 0,
    SingleInstanceIpc = 1,
    VideoSurface = 2,
    MpvEventPump = 3,
    MediaRelayListener = 4,
    MediaRelayReaper = 5,
    MediaRelayWorker = 6,
    SystemDnsWorker = 7,
    WasmEpochTimer = 8,
    ArtifactCleanup = 9,
    PluginCoordinator = 10,
    PluginStoreActor = 11,
    PluginPoolWorker = 12,
    PluginAuthCallback = 13,
    PluginOriginDns = 14,
    PluginSocketPump = 15,
    PluginRelayListener = 16,
    PluginRelayWorker = 17,
}

impl ThreadRole {
    pub const COUNT: usize = 18;

    pub const ALL: [Self; Self::COUNT] = [
        Self::ControlPipe,
        Self::SingleInstanceIpc,
        Self::VideoSurface,
        Self::MpvEventPump,
        Self::MediaRelayListener,
        Self::MediaRelayReaper,
        Self::MediaRelayWorker,
        Self::SystemDnsWorker,
        Self::WasmEpochTimer,
        Self::ArtifactCleanup,
        Self::PluginCoordinator,
        Self::PluginStoreActor,
        Self::PluginPoolWorker,
        Self::PluginAuthCallback,
        Self::PluginOriginDns,
        Self::PluginSocketPump,
        Self::PluginRelayListener,
        Self::PluginRelayWorker,
    ];

    /// Canonical OS thread name (or prefix for suffixed actor/pool roles).
    pub const fn base_name(self) -> &'static str {
        match self {
            Self::ControlPipe => "iris-control",
            Self::SingleInstanceIpc => "iris-open-files",
            Self::VideoSurface => "iris-video",
            Self::MpvEventPump => "iris-mpv-events",
            Self::MediaRelayListener => "iris-media-relay",
            Self::MediaRelayReaper => "iris-relay-reap",
            Self::MediaRelayWorker => "iris-media-request",
            Self::SystemDnsWorker => "iris-http-dns",
            Self::WasmEpochTimer => "iris-wasm-epoch",
            Self::ArtifactCleanup => "iris-artifact-cleanup",
            Self::PluginCoordinator => "iris-plugin-coordinator",
            Self::PluginStoreActor => "iris-v2-",
            Self::PluginPoolWorker => "iris-v2-",
            Self::PluginAuthCallback => "iris-v2-auth",
            Self::PluginOriginDns => "iris-origin-dns",
            Self::PluginSocketPump => "iris-v2-socket",
            Self::PluginRelayListener => "iris-v2-relay",
            Self::PluginRelayWorker => "iris-v2-range",
        }
    }

    /// Process-global thread cap enforced directly at spawn time for singleton
    /// and unscoped roles. Subsystem/Ledger-admitted roles return `None` here
    /// because their owning pool or `Ledger` enforces [`budget`] before spawn.
    const fn process_cap(self) -> Option<usize> {
        match self {
            Self::ControlPipe => Some(budget::CONTROL_PIPE_THREADS),
            Self::SingleInstanceIpc => Some(budget::SINGLE_INSTANCE_THREADS),
            Self::ArtifactCleanup => Some(budget::ARTIFACT_CLEANUP_THREADS_GLOBAL),
            Self::VideoSurface
            | Self::MpvEventPump
            | Self::MediaRelayListener
            | Self::MediaRelayReaper
            | Self::MediaRelayWorker
            | Self::SystemDnsWorker
            | Self::WasmEpochTimer
            | Self::PluginCoordinator
            | Self::PluginStoreActor
            | Self::PluginPoolWorker
            | Self::PluginAuthCallback
            | Self::PluginOriginDns
            | Self::PluginSocketPump
            | Self::PluginRelayListener
            | Self::PluginRelayWorker => None,
        }
    }
}

static LIVE_THREADS: [AtomicUsize; ThreadRole::COUNT] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

struct ThreadLease(ThreadRole);

impl ThreadLease {
    fn acquire(role: ThreadRole) -> io::Result<Self> {
        let slot = &LIVE_THREADS[role as usize];
        if let Some(cap) = role.process_cap() {
            slot.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < cap).then_some(n + 1)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("thread budget exceeded for {}", role.base_name()),
                )
            })?;
        } else {
            slot.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Self(role))
    }
}

impl Drop for ThreadLease {
    fn drop(&mut self) {
        LIVE_THREADS[self.0 as usize].fetch_sub(1, Ordering::AcqRel);
    }
}

/// Current number of live OS threads running under `role`.
pub fn live_threads(role: ThreadRole) -> usize {
    LIVE_THREADS[role as usize].load(Ordering::Acquire)
}

/// Spawns a registered OS thread with its canonical [`ThreadRole::base_name`].
pub fn spawn_thread<F, T>(role: ThreadRole, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_with_full_name(role, role.base_name().to_owned(), f)
}

/// Spawns a registered OS thread with a dynamic suffix appended to [`ThreadRole::base_name`].
pub fn spawn_thread_named<F, T>(role: ThreadRole, suffix: &str, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let mut name = String::with_capacity(role.base_name().len() + suffix.len());
    name.push_str(role.base_name());
    name.push_str(suffix);
    spawn_with_full_name(role, name, f)
}

fn spawn_with_full_name<F, T>(role: ThreadRole, name: String, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let lease = ThreadLease::acquire(role)?;
    thread::Builder::new().name(name).spawn(move || {
        let _lease = lease;
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::{CancelToken, Deadline, ThreadRole, WaitError, live_threads, spawn_thread};
    use std::io;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn cancel_token_propagates_to_children_and_deadline_enforces_timeouts() {
        let root = CancelToken::new();
        let child = root.child().unwrap();
        let linked = CancelToken::new();
        root.link(&linked).unwrap();

        let future = Deadline::after(Duration::from_secs(5));
        assert!(
            root.remaining_timeout(Some(future), Duration::from_secs(1))
                .unwrap()
                <= Duration::from_secs(1)
        );

        let expired = Deadline::after(Duration::ZERO);
        assert_eq!(root.check(Some(expired)), Err(WaitError::TimedOut));

        assert!(root.cancel());
        assert!(!root.cancel());
        assert!(child.is_cancelled());
        assert!(linked.is_cancelled());
        assert_eq!(root.check(Some(future)), Err(WaitError::Cancelled));
        assert!(root.child().is_err());
    }

    #[test]
    fn thread_registry_tracks_live_threads_and_enforces_singleton_caps() {
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let handle = spawn_thread(ThreadRole::ControlPipe, move || {
            worker_barrier.wait();
            worker_barrier.wait();
        })
        .unwrap();

        barrier.wait();
        assert_eq!(live_threads(ThreadRole::ControlPipe), 1);

        let second = spawn_thread(ThreadRole::ControlPipe, || {});
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::WouldBlock);

        barrier.wait();
        handle.join().unwrap();
        assert_eq!(live_threads(ThreadRole::ControlPipe), 0);
    }
}
