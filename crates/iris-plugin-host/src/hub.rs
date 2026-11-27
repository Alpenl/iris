//! Single-window coordinator and single-lock [`Inbox`]. UI entry points only perform
//! bounded validation/enqueue; compilation, disk and playback setup use workers.
mod client;
mod dispatch;
mod jobs;
mod origin;
mod worker;
use super::{
    actor::Update,
    installation::{ArchivedIdentity, Installed, Installer},
    ledger::{Reservation, Resource},
    requests::{AcceptedPage, RequestTicket, SourceRequests},
    runtime::{Control, Setup},
    stream::prepare::Playback,
};
pub(crate) use client::Client;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostEvent, Id, Manifest,
    source::{PlaybackSnapshot, PlaylistEntry, SourceDescriptor},
};
pub use origin::OriginSummary;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct Config {
    pub installer: Installer,
    pub bootstrap: Option<super::official::Bootstrap>,
    pub runtime: Setup,
    pub disabled: BTreeSet<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    Dormant,
    Loading,
    Ready,
    Disabled,
    Stopping,
    Failed(ErrorCode),
}
/// Revocation is observable even after a batch left the coordinator/UI queue.
pub struct Binding {
    pub installed: Installed,
    pub manifest: Arc<Manifest>,
    alive: AtomicBool,
    _lease: Reservation,
}
impl Binding {
    pub fn is_live(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
#[derive(Clone)]
pub struct PluginInfo {
    pub id: String,
    pub enabled: bool,
    pub status: PluginStatus,
    pub binding: Option<Arc<Binding>>,
    pub sources: Option<Arc<SourceSnapshot>>,
}
pub struct SourceSnapshot {
    pub sources: Vec<SourceDescriptor>,
    _lease: Reservation,
}
pub(crate) struct Catalog {
    pub revision: u64,
    pub plugins: Vec<PluginInfo>,
}
pub struct ReviewSummary {
    pub id: Id,
    pub manifest: Manifest,
    pub digest: [u8; 32],
    pub previous: Option<Installed>,
    pub archived: Option<ArchivedIdentity>,
    _lease: Reservation,
}
pub struct Commands {
    pub(crate) binding: Arc<Binding>,
    pub(crate) update: Arc<Update>,
    /// Source results are consumed by the coordinator; only these indices may
    /// be applied by the UI. The Arc retains all decoded-message reservations.
    pub(crate) indices: Vec<usize>,
}
pub enum Notice {
    Commands(Commands),
    Page {
        binding: Arc<Binding>,
        generation: Generation,
        request: Id,
        page: Arc<AcceptedPage>,
    },
    Playback {
        binding: Arc<Binding>,
        generation: Generation,
        ticket: Arc<RequestTicket>,
        playback: Playback,
        playlist: Vec<PlaylistEntry>,
    },
    Error {
        plugin: Option<String>,
        request: Option<Id>,
        code: ErrorCode,
        message: Option<String>,
    },
    Review(Arc<ReviewSummary>),
    Origin(Arc<OriginSummary>),
    OriginClosed(Id),
    Installed(Installed),
    Uninstalled(String),
    Browser {
        binding: Arc<Binding>,
        generation: Generation,
        transaction: Id,
        url: String,
    },
}

/// Consistent single-lock snapshot of [`InboxState`].
pub(super) struct InboxSnapshot {
    pub generation: Generation,
    pub closed: bool,
    pub access: BTreeMap<String, super::runtime::WindowAccess>,
}

pub(super) struct InboxState {
    pub generation: Generation,
    pub ready: BTreeSet<String>,
    pub access: BTreeMap<String, super::runtime::WindowAccess>,
    pub requests: SourceRequests,
    pub cancelled: BTreeMap<String, BTreeSet<Id>>,
    pub faults: BTreeMap<String, ErrorCode>,
    pub notices: VecDeque<Notice>,
    pub closed: bool,
    pub wake_pending: bool,
}

struct Inbox {
    id: Id,
    state: Mutex<InboxState>,
    wake: UnboundedSender<()>,
    _lease: Reservation,
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> InboxSnapshot {
        let state = self.lock();
        InboxSnapshot {
            generation: state.generation,
            closed: state.closed,
            access: state.access.clone(),
        }
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn notify(&self) {
        let should_wake = {
            let mut state = self.lock();
            if state.closed || state.wake_pending {
                false
            } else {
                state.wake_pending = true;
                true
            }
        };
        if should_wake {
            let _ = self.wake.unbounded_send(());
        }
    }

    fn push(&self, notice: Notice) -> Result<(), ErrorCode> {
        let should_wake = {
            let mut state = self.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            if state.notices.len() >= 32 {
                return Err(ErrorCode::Backpressure);
            }
            state.notices.push_back(notice);
            if state.wake_pending {
                false
            } else {
                state.wake_pending = true;
                true
            }
        };
        if should_wake {
            let _ = self.wake.unbounded_send(());
        }
        Ok(())
    }

    fn generation(&self) -> Generation {
        self.lock().generation
    }
}

enum Command {
    Snapshot {
        window: Id,
        snapshot: PlaybackSnapshot,
    },
    Open {
        inbox: Arc<Inbox>,
        snapshot: Option<PlaybackSnapshot>,
    },
    Advance {
        window: Id,
        generation: Generation,
    },
    Send {
        window: Id,
        plugin: String,
        event: HostEvent<serde_json::Value>,
        generation: Generation,
        request: Option<Arc<RequestTicket>>,
    },
    Enabled {
        plugin: String,
        enabled: bool,
    },
    Uninstall {
        window: Id,
        plugin: String,
        keep_data: bool,
    },
    Reload {
        disabled: Option<BTreeSet<String>>,
    },
    SyncDisabled {
        disabled: BTreeSet<String>,
    },
    Review {
        window: Id,
        id: Id,
        path: PathBuf,
    },
    Approve {
        window: Id,
        id: Id,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    },
    DismissReview {
        window: Id,
        id: Id,
    },
    ConfirmOrigin {
        window: Id,
        id: Id,
        allow: bool,
        insecure_auth: bool,
    },
}

#[derive(Default)]
struct Gate {
    wake: bool,
    shutdown: bool,
    commands: VecDeque<Command>,
    completed: VecDeque<jobs::Completed>,
    window: Option<(Id, Weak<Inbox>)>,
}

struct Shared {
    gate: Mutex<Gate>,
    changed: Condvar,
    catalog: Mutex<Arc<Catalog>>,
    ledger: super::ledger::Ledger,
    _lease: Reservation,
}

impl Shared {
    fn signal(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.wake = true;
        self.changed.notify_one();
    }

    fn enqueue(&self, command: Command) -> Result<(), ErrorCode> {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.shutdown {
            return Err(ErrorCode::Cancelled);
        }
        if gate.commands.len() >= 64 {
            return Err(ErrorCode::Backpressure);
        }
        gate.commands.push_back(command);
        gate.wake = true;
        self.changed.notify_one();
        Ok(())
    }
}

struct Connection(Arc<Shared>);
impl Drop for Connection {
    fn drop(&mut self) {
        let mut gate = self.0.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.shutdown = true;
        gate.wake = true;
        self.0.changed.notify_one();
    }
}

#[derive(Clone)]
pub(crate) struct Hub(Arc<Connection>);
impl Hub {
    pub(crate) fn new(mut config: Config) -> Result<Self, ErrorCode> {
        let ledger = config.runtime.resources.ledger.clone();
        let lease = ledger.reserve("host-coordinator", &[(Resource::HostBytes, 32 << 20)])?;
        let shared = Arc::new(Shared {
            gate: Mutex::new(Gate::default()),
            changed: Condvar::new(),
            catalog: Mutex::new(Arc::new(Catalog {
                revision: 0,
                plugins: Vec::new(),
            })),
            ledger,
            _lease: lease,
        });
        let weak = Arc::downgrade(&shared);
        config.runtime.wake = Arc::new(move || {
            if let Some(shared) = weak.upgrade() {
                shared.signal();
            }
        });
        let worker = worker::Worker::new(shared.clone(), config)?;
        iris_base::rt::spawn_thread(iris_base::rt::ThreadRole::PluginCoordinator, move || {
            worker.run()
        })
        .map_err(|_| ErrorCode::Internal)?;
        Ok(Self(Arc::new(Connection(shared))))
    }

    pub(crate) fn open_window(
        &self,
        generation: Generation,
        snapshot: Option<PlaybackSnapshot>,
    ) -> Result<(Client, UnboundedReceiver<()>), ErrorCode> {
        if let Some(snapshot) = &snapshot {
            iris_core::bounded_json::encode(snapshot, iris_api::v2::MESSAGE_LIMIT - 1024)
                .map_err(super::json_error)?;
        }
        let shared = &self.0.0;
        let id = Control::new_window_id()?;
        let lease = shared
            .ledger
            .reserve("host-coordinator", &[(Resource::HostBytes, 4 << 20)])?;
        let (wake, rx) = unbounded();
        let inbox = Arc::new(Inbox {
            id,
            state: Mutex::new(InboxState {
                generation,
                ready: BTreeSet::new(),
                access: BTreeMap::new(),
                requests: SourceRequests::new(shared.ledger.clone()),
                cancelled: BTreeMap::new(),
                faults: BTreeMap::new(),
                notices: VecDeque::new(),
                closed: false,
                wake_pending: false,
            }),
            wake,
            _lease: lease,
        });
        let mut gate = shared.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate
            .window
            .as_ref()
            .is_some_and(|(_, w)| w.upgrade().is_none_or(|w| w.is_closed()))
        {
            gate.window = None;
        }
        if gate.shutdown {
            return Err(ErrorCode::Cancelled);
        }
        if gate.window.is_some() || gate.commands.len() >= 64 {
            return Err(ErrorCode::Backpressure);
        }
        gate.window = Some((id, Arc::downgrade(&inbox)));
        gate.commands.push_back(Command::Open {
            inbox: inbox.clone(),
            snapshot,
        });
        gate.wake = true;
        shared.changed.notify_one();
        Ok((Client::connected(self.clone(), inbox), rx))
    }

    pub(crate) fn sync_disabled(&self, disabled: BTreeSet<String>) -> Result<(), ErrorCode> {
        self.0.0.enqueue(Command::SyncDisabled { disabled })
    }

    pub(crate) fn catalog(&self) -> Arc<Catalog> {
        self.0
            .0
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests;
