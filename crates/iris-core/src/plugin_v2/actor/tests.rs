use super::*;
use std::sync::atomic::AtomicBool;
mod windows;

struct Reject;
impl Executor for Reject {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        _: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}
fn mailbox() -> Arc<Mailbox> {
    Arc::new(
        Mailbox::new(
            "fixture".into(),
            Role::Stream,
            Generation::default(),
            BTreeSet::new(),
            Ledger::default(),
            Arc::new(Reject),
            Arc::new(|| {}),
        )
        .unwrap(),
    )
}
fn blob(shared: &Mailbox, bytes: &[u8]) -> Arc<Blob> {
    Arc::new(Blob::new(&shared.ledger, &shared.owner, bytes.to_vec()).unwrap())
}

#[test]
fn eof_duplicate_and_seek_are_checked_before_consuming_data() {
    let shared = mailbox();
    let ticket = shared.read("resource".into(), 1, 4, Some(8)).unwrap();
    assert_eq!(
        shared
            .reply(ticket.request, blob(&shared, b"1234"), true)
            .unwrap_err(),
        ErrorCode::RangeInvalid
    );
    assert_eq!(
        shared
            .reply(ticket.request, blob(&shared, b"12"), false)
            .unwrap(),
        2
    );
    assert_eq!(
        ticket
            .receive(Duration::from_millis(10))
            .unwrap()
            .blob
            .bytes,
        b"12"
    );
    assert_eq!(
        shared
            .reply(ticket.request, blob(&shared, b"1"), false)
            .unwrap_err(),
        ErrorCode::StaleHandle
    );
    let stale = shared.read("resource".into(), 0, 1, Some(8)).unwrap();
    shared
        .advance(Generation {
            seek: Id(1),
            ..Generation::default()
        })
        .unwrap();
    assert!(matches!(
        stale.receive(Duration::from_millis(10)),
        Err(ErrorCode::Cancelled)
    ));
    assert_eq!(
        shared
            .reply(stale.request, blob(&shared, b"1"), false)
            .unwrap_err(),
        ErrorCode::StaleHandle
    );
    let eof = shared.read("resource".into(), 8, 1, Some(8)).unwrap();
    assert!(
        shared
            .reply(eof.request, blob(&shared, b""), false)
            .is_err()
    );
    assert_eq!(
        shared.reply(eof.request, blob(&shared, b""), true).unwrap(),
        0
    );
}

#[test]
fn context_refresh_keeps_bound_reads_but_rejects_rollback() {
    let shared = mailbox();
    let ticket = shared.read("resource".into(), 0, 1, None).unwrap();
    shared
        .advance(Generation {
            context: Id(2),
            ..Generation::default()
        })
        .unwrap();
    assert_eq!(
        shared
            .reply(ticket.request, blob(&shared, b"a"), false)
            .unwrap(),
        1
    );
    assert!(
        shared
            .advance(Generation {
                context: Id(1),
                ..Generation::default()
            })
            .is_err()
    );
}

#[test]
fn queue_full_and_close_release_reservations_without_guest_execution() {
    let shared = mailbox();
    for _ in 0..32 {
        shared.send(b"{}".to_vec(), None).unwrap();
    }
    assert_eq!(
        shared.send(b"{}".to_vec(), None).unwrap_err(),
        ErrorCode::Backpressure
    );
    assert!(shared.ledger.used(None, Resource::HostBytes) > 0);
    shared.stop();
    assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
    assert!(shared.next().is_none());
    assert_eq!(
        shared.send(b"{}".to_vec(), None).unwrap_err(),
        ErrorCode::Cancelled
    );
}

#[test]
fn store_slots_are_reserved_before_spawning_threads() {
    let engine = RuntimeEngine::new().unwrap();
    let wasm = wat::parse_str(
        r#"(module (memory (export "memory") 1)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 8)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#,
    )
    .unwrap();
    let module = engine.compile(&wasm, &AtomicBool::new(false)).unwrap();
    let ledger = Ledger::default();
    let io = IoPool::new().unwrap();
    let config = || Spawn {
        engine: engine.clone(),
        module: module.clone(),
        role: Role::Control,
        owner: "p".into(),
        grants: BTreeSet::new(),
        ledger: ledger.clone(),
        io: io.clone(),
        executor: Arc::new(Reject),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    };
    let actor = Actor::spawn(config()).unwrap();
    assert!(Actor::spawn(config()).is_err());
    actor.stop();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !actor.is_quiescent() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(actor.is_quiescent());
    assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
}

#[test]
fn read_cancellation_propagates_and_holds_slots_until_io_exits() {
    let shared = mailbox();
    let reads: Vec<_> = (0..4)
        .map(|_| shared.read("av".into(), 0, 1, None).unwrap())
        .collect();
    let mut contexts = Vec::new();
    for read in &reads {
        contexts.push(shared.io_context(Some(Id(read.request)), None).unwrap());
    }
    let unaffected = contexts.pop().unwrap();
    for read in reads.iter().take(3) {
        shared.cancel_read(read.request);
        assert!(!shared.has_read(Id(read.request)));
    }
    assert!(contexts.iter().all(|c| c.cancel.is_cancelled()));
    assert!(!unaffected.cancel.is_cancelled());
    assert!(matches!(
        shared.read("av".into(), 0, 1, None),
        Err(ErrorCode::Backpressure)
    ));
    // Drain four initial events and three cancellation events. Pending I/O owns
    // the original read slots even after the guest sees cancellation.
    for _ in 0..7 {
        drop(shared.next().unwrap());
    }
    assert_eq!(shared.ledger.used(None, Resource::StreamRead), 4);
    drop(contexts);
    assert_eq!(shared.ledger.used(None, Resource::StreamRead), 1);
    let new = shared.read("av".into(), 0, 1, None).unwrap();
    shared.stop();
    assert!(unaffected.cancel.is_cancelled());
    drop(unaffected);
    drop(new);
    drop(reads);
    assert_eq!(shared.ledger.used(None, Resource::StreamRead), 0);
    assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn operation_cancel_preserves_slots_and_discards_undelivered_handles() {
    struct Paused {
        started: mpsc::SyncSender<HttpCancel>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        discarded: Arc<AtomicBool>,
    }
    impl Executor for Paused {
        fn prepare(
            &self,
            _: Task,
            _: &BTreeMap<u64, Arc<Blob>>,
            context: TaskContext,
        ) -> Result<Operation, ErrorCode> {
            let started = self.started.clone();
            let release = self.release.clone();
            let discarded = self.discarded.clone();
            Ok(Operation::blocking(move || {
                started.send(context.cancel).unwrap();
                let (lock, changed) = &*release;
                let ready = lock.lock().unwrap();
                let (ready, _) = changed
                    .wait_timeout_while(ready, Duration::from_secs(3), |v| !*v)
                    .unwrap();
                assert!(*ready, "test did not release operation");
                Ok(
                    IoOutcome::new(serde_json::json!({"handle":"1"}), None).on_discard(move || {
                        discarded.store(true, Ordering::Release);
                    }),
                )
            }))
        }
        fn close(&self, _: u64) -> Result<(), ErrorCode> {
            Err(ErrorCode::StaleHandle)
        }
        fn stop(&self) {}
    }
    let (started, rx) = mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let discarded = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(
        Mailbox::new(
            "cancel-test".into(),
            Role::Control,
            Generation::default(),
            BTreeSet::new(),
            Ledger::default(),
            Arc::new(Paused {
                started,
                release: release.clone(),
                discarded: discarded.clone(),
            }),
            Arc::new(|| {}),
        )
        .unwrap(),
    );
    let mut bridge = Bridge {
        shared: shared.clone(),
        io: IoPool::new().unwrap(),
        generation: Generation::default(),
        read: None,
        window: None,
        request_cancel: None,
    };
    let operation = bridge
        .submit(
            Task::HttpRead {
                handle: Id(1),
                len: 1,
            },
            &BTreeMap::new(),
        )
        .unwrap();
    let cancel = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    bridge.close(operation).unwrap();
    assert!(cancel.is_cancelled());
    assert_eq!(bridge.close(operation), Err(ErrorCode::StaleHandle));
    assert_eq!(shared.ledger.used(None, Resource::Task), 1);
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    let message = shared.next().unwrap();
    assert!(matches!(
        &message,
        Message::Completed {
            result: Err(ErrorCode::Cancelled),
            ..
        }
    ));
    assert!(discarded.load(Ordering::Acquire));
    assert_eq!(shared.ledger.used(None, Resource::Task), 1);
    drop(message);
    assert_eq!(shared.ledger.used(None, Resource::Task), 0);
    assert_eq!(shared.outstanding.load(Ordering::Acquire), 0);
    assert_eq!(bridge.close(operation), Err(ErrorCode::StaleHandle));
    shared.stop();
}
