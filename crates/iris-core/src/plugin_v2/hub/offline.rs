use super::{Catalog, Client, Connection, Gate, Hub, Inbox, Notice, Shared};
use crate::plugin_v2::{ledger::Resource, requests::SourceRequests};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_api::v2::{ErrorCode, Generation, Id};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool},
};

impl Client {
    /// A bounded error-only client, without workers, engines or disk access.
    pub(crate) fn unavailable(code: ErrorCode) -> (Self, UnboundedReceiver<()>) {
        let ledger = super::super::ledger::Ledger::default();
        // A fresh ledger has no charges and its fixed limits exceed these tiny reservations.
        let lease = ledger
            .reserve("unavailable", &[(Resource::HostBytes, 4096)])
            .expect("fresh ledger admits error state");
        let inbox_lease = ledger
            .reserve("unavailable", &[(Resource::HostBytes, 4096)])
            .expect("fresh ledger admits error inbox");
        let shared = Arc::new(Shared {
            gate: Mutex::new(Gate {
                shutdown: true,
                ..Gate::default()
            }),
            changed: Condvar::new(),
            catalog: Mutex::new(Arc::new(Catalog {
                revision: 0,
                plugins: Vec::new(),
            })),
            ledger: ledger.clone(),
            _lease: lease,
        });
        let (wake, rx) = unbounded();
        let inbox = Arc::new(Inbox {
            id: Id(0),
            generation: Mutex::new(Generation::default()),
            ready: Mutex::new(BTreeSet::new()),
            access: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(SourceRequests::new(ledger)),
            cancelled: Mutex::new(BTreeMap::new()),
            faults: Mutex::new(BTreeMap::new()),
            notices: Mutex::new(VecDeque::from([Notice::Error {
                plugin: None,
                request: None,
                code,
                message: Some("插件运行时不可用".into()),
            }])),
            closed: AtomicBool::new(false),
            wake_pending: AtomicBool::new(false),
            wake,
            _lease: inbox_lease,
        });
        inbox.notify();
        (
            Self {
                hub: Hub(Arc::new(Connection(shared))),
                inbox,
            },
            rx,
        )
    }
}
