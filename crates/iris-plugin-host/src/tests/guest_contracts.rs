//! White-box `Instance`, `Actor`, and `StreamStore` contract tests executed
//! against compiled Rust and freestanding Zig-C Wasm guests (`scripts/test-plugin-v2.py`).

use crate::{
    actor::{Actor, Completion, Executor, IoOutcome, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    instance::{Blob, Instance, InstanceConfig},
    ledger::{Ledger, Resource},
    network::NetworkGrant,
    pool::IoPool,
    stream::{
        guest::StreamStore,
        relay::BoundRelay,
        remote::RemoteFactory,
        session::{StreamSession, new_session_id},
    },
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostCommand, Id, NetworkDeclaration, Reply, Role,
    stream::StreamContext, task::Task,
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::CancelToken;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn guest(name: &str) -> Vec<u8> {
    std::fs::read(
        std::env::var(name).expect("run scripts/test-plugin-v2.py to compile language fixtures"),
    )
    .unwrap()
}

fn until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let end = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires compiled Rust/C guests; run scripts/test-plugin-v2.py"]
fn rust_and_c_share_decimal_and_ownership_vectors() {
    let engine = RuntimeEngine::new().unwrap();
    for name in ["IRIS_V2_RUST_GUEST", "IRIS_V2_C_GUEST"] {
        let cancel = CancelToken::new();
        let module = engine.compile(&guest(name), &cancel).unwrap();
        let mut instance = Instance::new(InstanceConfig {
            engine: engine.clone(),
            module,
            role: Role::Control,
            owner: name.into(),
            grants: BTreeSet::new(),
            ledger: Ledger::default(),
            cancel,
        })
        .unwrap();
        for _ in 0..50 {
            for id in [
                "0",
                "9007199254740993",
                "9223372036854775807",
                "18446744073709551615",
            ] {
                let bytes =
                    serde_json::to_vec(&serde_json::json!({"vector":"decimal","id":id})).unwrap();
                let Reply::Ok { ok } = instance.on_event(&bytes).unwrap() else {
                    panic!("failed {name}");
                };
                assert!(
                    matches!(ok.as_slice(), [HostCommand::ShowToast { message }] if message == id)
                );
            }
        }
        for id in ["01", "+1", "-1", "18446744073709551616", ""] {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"vector":"decimal","id":id})).unwrap();
            assert!(
                matches!(instance.on_event(&bytes), Ok(Reply::Error { error }) if error.code == -1)
            );
        }
    }
}

struct Delay {
    started: Arc<AtomicBool>,
    cleanup_delay: Duration,
}

impl Executor for Delay {
    fn prepare(
        &self,
        task: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if !matches!(task, Task::HttpStart { .. }) {
            return Err(ErrorCode::Unsupported);
        }
        let started = self.started.clone();
        let cleanup_delay = self.cleanup_delay;
        Ok(Operation::blocking(move || {
            started.store(true, Ordering::Release);
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if context.cancel.is_cancelled() {
                    std::thread::sleep(cleanup_delay);
                    return Err(ErrorCode::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(IoOutcome::new(serde_json::json!({"status":200}), None))
        }))
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn control_io_and_dead_loop_do_not_block_four_stream_reads() {
    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&guest("IRIS_V2_RUST_GUEST"), &CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let io = IoPool::new().unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let config = |role| Spawn {
        engine: engine.clone(),
        module: module.clone(),
        role,
        owner: "dual".into(),
        grants: BTreeSet::from([Capability::NetHttp, Capability::Notifications]),
        ledger: ledger.clone(),
        io: io.clone(),
        executor: Arc::new(Delay {
            started: started.clone(),
            cleanup_delay: Duration::ZERO,
        }),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    };
    let control = Actor::spawn(config(Role::Control)).unwrap();
    let stream = Actor::spawn(config(Role::Stream)).unwrap();
    control.send(br#"{"event":"slow"}"#.to_vec()).unwrap();
    until(|| started.load(Ordering::Acquire), Duration::from_secs(2));
    let start = Instant::now();
    let reads: Vec<_> = (0..4)
        .map(|i| stream.read("av".into(), i * 64, 64, None).unwrap())
        .collect();
    for (i, read) in reads.iter().enumerate() {
        let result = read.receive(Duration::from_secs(2)).unwrap();
        assert_eq!(
            result.blob.bytes,
            (0..64).map(|n| (i * 64 + n) as u8).collect::<Vec<_>>()
        );
    }
    assert!(start.elapsed() < Duration::from_secs(3));
    let context = Arc::new(Blob::new(&ledger, "dual", vec![0xa5]).unwrap());
    stream
        .send_context(
            serde_json::json!({"event":"stream_init","session":"1","generation":Generation::default()}),
            context,
        )
        .unwrap();
    let result = stream
        .read("av".into(), 32, 64, None)
        .unwrap()
        .receive(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        result.blob.bytes,
        (32..96).map(|v| v ^ 0xa5).collect::<Vec<u8>>()
    );
    drop(result);
    assert_eq!(control.status(), Status::Ready);
    until(|| control.poll().is_some(), Duration::from_secs(6));
    control.send(br#"{"event":"spin"}"#.to_vec()).unwrap();
    assert!(
        stream
            .read("av".into(), 256, 64, None)
            .unwrap()
            .receive(Duration::from_secs(1))
            .is_ok()
    );
    until(
        || matches!(control.status(), Status::Failed(ErrorCode::Timeout)),
        Duration::from_secs(3),
    );
    assert!(
        stream
            .read("av".into(), 320, 64, None)
            .unwrap()
            .receive(Duration::from_secs(1))
            .is_ok()
    );
    drop(reads);
    control.stop();
    stream.stop();
    until(
        || control.is_quiescent() && stream.is_quiescent(),
        Duration::from_secs(2),
    );
    assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
    assert_eq!(ledger.used(None, Resource::Task), 0);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn cancelling_does_not_release_running_io_quota_early() {
    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&guest("IRIS_V2_RUST_GUEST"), &CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let started = Arc::new(AtomicBool::new(false));
    let actor = Actor::spawn(Spawn {
        engine,
        module,
        role: Role::Control,
        owner: "cancel".into(),
        grants: BTreeSet::from([Capability::NetHttp]),
        ledger: ledger.clone(),
        io: IoPool::new().unwrap(),
        executor: Arc::new(Delay {
            started: started.clone(),
            cleanup_delay: Duration::from_millis(250),
        }),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    actor.send(br#"{"event":"slow"}"#.to_vec()).unwrap();
    until(|| started.load(Ordering::Acquire), Duration::from_secs(2));
    let start = Instant::now();
    actor.stop();
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(ledger.used(None, Resource::Task), 1);
    assert!(!actor.is_quiescent());
    until(|| actor.is_quiescent(), Duration::from_secs(2));
    assert_eq!(ledger.used(None, Resource::Task), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn random_access_relay_refresh_preserves_inflight_context_and_cancellation() {
    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&guest("IRIS_V2_RUST_GUEST"), &CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let started = Arc::new(AtomicBool::new(false));
    let context = StreamContext {
        version: Id(1),
        blob: Id(1),
        origin_grants: Vec::new(),
        transports: vec![Capability::NetHttp],
        expires_unix_ms: None,
    };
    let id = new_session_id().unwrap();
    let store = Arc::new(
        StreamStore::spawn(
            Spawn {
                engine,
                module,
                role: Role::Stream,
                owner: "stream-context".into(),
                grants: BTreeSet::from([Capability::NetHttp]),
                ledger: ledger.clone(),
                io: IoPool::new().unwrap(),
                executor: Arc::new(Delay {
                    started: started.clone(),
                    cleanup_delay: Duration::from_millis(250),
                }),
                generation: Generation::default(),
                wake: Arc::new(|| {}),
            },
            id,
            &context,
            Arc::new(Blob::new(&ledger, "stream-context", vec![0xa5]).unwrap()),
        )
        .unwrap(),
    );
    let delayed = store.reader("delayed".into(), Some(512)).unwrap();
    let delayed_thread =
        std::thread::spawn(move || delayed.read_at(8, 32, &HttpCancel::default()).unwrap());
    until(|| started.load(Ordering::Acquire), Duration::from_secs(2));
    let updated = StreamContext {
        version: Id(2),
        ..context.clone()
    };
    store
        .refresh(
            Id(1),
            &updated,
            Arc::new(Blob::new(&ledger, "stream-context", vec![0x3c]).unwrap()),
        )
        .unwrap();
    assert!(
        store
            .refresh(
                Id(1),
                &context,
                Arc::new(Blob::new(&ledger, "stream-context", vec![0]).unwrap())
            )
            .is_err()
    );
    let grant = NetworkGrant::new(
        "stream-context".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let factory = RemoteFactory::new(grant.clone(), ledger.clone()).unwrap();
    let bound = BoundRelay::bind().unwrap();
    let session = StreamSession::new(
        id,
        Generation::default(),
        bound.authority().unwrap(),
        factory,
        Some(store.clone()),
    )
    .unwrap();
    session
        .commit(
            session
                .prepare_reader(store.reader("av".into(), Some(512)).unwrap())
                .unwrap(),
        )
        .unwrap();
    let relay = bound.start(session.clone()).unwrap();
    let url = url::Url::parse(&session.root_url().unwrap()).unwrap();
    let mut client = TcpStream::connect(session.authority()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {}\r\nRange: bytes=17-48\r\n\r\n",
        url.path(),
        session.authority()
    )
    .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    assert!(response.starts_with(b"HTTP/1.1 206"));
    assert!(response.ends_with(&(17..49).map(|n| n ^ 0x3c).collect::<Vec<u8>>()));
    let old = delayed_thread.join().unwrap();
    assert_eq!(
        old.blob.bytes,
        (8..40).map(|n| n ^ 0xa5).collect::<Vec<u8>>()
    );
    drop(old);
    started.store(false, Ordering::Release);
    let cancel = HttpCancel::default();
    let worker_cancel = cancel.clone();
    let delayed = store.reader("delayed".into(), Some(512)).unwrap();
    let waiter = std::thread::spawn(move || delayed.read_at(0, 32, &worker_cancel));
    until(|| started.load(Ordering::Acquire), Duration::from_secs(2));
    cancel.cancel();
    assert!(matches!(waiter.join().unwrap(), Err(ErrorCode::Cancelled)));
    assert_eq!(ledger.used(None, Resource::Task), 1);
    assert_eq!(ledger.used(None, Resource::StreamRead), 1);
    relay.stop();
    until(|| relay.is_quiescent(), Duration::from_secs(2));
    drop(relay);
    drop(session);
    drop(store);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::Task), 0);
    assert_eq!(ledger.used(None, Resource::StreamRead), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
}

type Pending = Arc<Mutex<BTreeMap<Id, (TaskContext, Completion)>>>;

struct WindowIo(Pending);

impl Executor for WindowIo {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        let pending = self.0.clone();
        Ok(Operation::deferred(move |done| {
            pending
                .lock()
                .unwrap()
                .insert(context.window.unwrap(), (context, done));
            Ok(())
        }))
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn single_window_enforces_exclusivity_cancellation_and_clean_reopen() {
    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&guest("IRIS_V2_RUST_GUEST"), &CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let pending = Pending::default();
    let actor = Actor::spawn(Spawn {
        engine,
        module,
        role: Role::Control,
        owner: "singlewindow".into(),
        grants: BTreeSet::from([Capability::NetHttp, Capability::Notifications]),
        ledger: ledger.clone(),
        io: IoPool::new().unwrap(),
        executor: Arc::new(WindowIo(pending.clone())),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    until(|| actor.status() == Status::Ready, Duration::from_secs(5));
    assert_eq!(ledger.used(None, Resource::ControlStore), 1);
    let first = Generation {
        media: Id(1),
        ..Generation::default()
    };
    let next = Generation {
        seek: Id(1),
        ..first
    };
    actor.open_window(Id(101), first).unwrap();
    assert_eq!(
        actor.open_window(Id(102), first),
        Err(ErrorCode::QuotaExceeded)
    );
    let slow = br#"{"event":"command","id":"slow","snapshot":null}"#.to_vec();
    actor.send_window(Id(101), first, slow.clone()).unwrap();
    until(
        || pending.lock().unwrap().len() == 1,
        Duration::from_secs(3),
    );
    actor.advance_window(Id(101), next).unwrap();
    assert!(pending.lock().unwrap()[&Id(101)].0.cancel.is_cancelled());
    assert_eq!(ledger.used(None, Resource::Task), 1);
    let (_, done_a) = pending.lock().unwrap().remove(&Id(101)).unwrap();
    done_a(Ok(IoOutcome::new(serde_json::json!({"status":200}), None)));
    actor
        .send_window(
            Id(101),
            next,
            br#"{"event":"command","id":"ping","snapshot":null}"#.to_vec(),
        )
        .unwrap();
    let mut received = None;
    until(
        || {
            received = actor.poll();
            received.is_some()
        },
        Duration::from_secs(3),
    );
    let update = received.unwrap();
    assert_eq!(update.window, Some(Id(101)));
    assert_eq!(update.generation, next);
    assert!(
        matches!(update.commands.as_slice(), [HostCommand::ShowToast { message }] if message == "pong")
    );
    drop(update);

    actor.close_window(Id(101)).unwrap();
    actor.open_window(Id(102), first).unwrap();
    actor.send_window(Id(102), first, slow).unwrap();
    until(
        || pending.lock().unwrap().contains_key(&Id(102)),
        Duration::from_secs(3),
    );
    let (_, done_b) = pending.lock().unwrap().remove(&Id(102)).unwrap();
    done_b(Ok(IoOutcome::new(serde_json::json!({"status":200}), None)));
    let mut received = None;
    until(
        || {
            received = actor.poll();
            received.is_some()
        },
        Duration::from_secs(3),
    );
    let update = received.unwrap();
    assert_eq!(update.window, Some(Id(102)));
    assert_eq!(update.generation, first);
    assert!(
        matches!(update.commands.as_slice(), [HostCommand::ShowToast { message }] if message == "completed")
    );
    drop(update);
    actor.close_window(Id(102)).unwrap();
    actor.stop();
    until(|| actor.is_quiescent(), Duration::from_secs(3));
    for resource in [
        Resource::Task,
        Resource::Handle,
        Resource::ControlStore,
        Resource::HostBytes,
        Resource::WasmBytes,
    ] {
        assert_eq!(ledger.used(None, resource), 0, "{resource:?}");
    }
}

type PendingRequests = Arc<Mutex<Vec<(HttpCancel, Completion)>>>;

struct RequestIo(PendingRequests);

impl Executor for RequestIo {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        let pending = self.0.clone();
        Ok(Operation::deferred(move |done| {
            pending.lock().unwrap().push((context.cancel, done));
            Ok(())
        }))
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn individual_source_request_cancel_survives_async_callbacks_without_cancelling_its_neighbor() {
    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&guest("IRIS_V2_RUST_GUEST"), &CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let pending = PendingRequests::default();
    let actor = Actor::spawn(Spawn {
        engine,
        module,
        role: Role::Control,
        owner: "request-scope".into(),
        grants: BTreeSet::from([Capability::NetHttp, Capability::Notifications]),
        ledger: ledger.clone(),
        io: IoPool::new().unwrap(),
        executor: Arc::new(RequestIo(pending.clone())),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    actor.open_window(Id(1), Generation::default()).unwrap();
    let a = HttpCancel::default();
    let b = HttpCancel::default();
    let slow = br#"{"event":"command","id":"slow","snapshot":null}"#.to_vec();
    actor
        .send_cancellable(Id(1), Generation::default(), slow.clone(), a.clone())
        .unwrap();
    actor
        .send_cancellable(Id(1), Generation::default(), slow.clone(), b.clone())
        .unwrap();
    until(
        || pending.lock().unwrap().len() == 2,
        Duration::from_secs(3),
    );
    a.cancel();
    assert!(pending.lock().unwrap()[0].0.is_cancelled());
    assert!(!pending.lock().unwrap()[1].0.is_cancelled());
    assert_eq!(ledger.used(None, Resource::Task), 2);
    let ready = std::mem::take(&mut *pending.lock().unwrap());
    for (_, done) in ready {
        done(Ok(IoOutcome::new(serde_json::json!({"status":200}), None)));
    }
    let mut update = None;
    until(
        || {
            update = actor.poll();
            update.is_some()
        },
        Duration::from_secs(3),
    );
    let update = update.unwrap();
    assert!(
        matches!(update.commands.as_slice(), [HostCommand::ShowToast { message }] if message == "completed")
    );
    assert!(!update.is_cancelled());
    b.cancel();
    assert!(
        update.is_cancelled(),
        "the UI can detect cancellation after dequeue"
    );
    drop(update);
    assert_eq!(
        actor.send_cancellable(Id(1), Generation::default(), slow, a),
        Err(ErrorCode::Cancelled)
    );
    actor
        .send_window(
            Id(1),
            Generation::default(),
            br#"{"event":"command","id":"ping","snapshot":null}"#.to_vec(),
        )
        .unwrap();
    let mut update = None;
    until(
        || {
            update = actor.poll();
            update.is_some()
        },
        Duration::from_secs(3),
    );
    assert!(
        !update.unwrap().is_cancelled(),
        "the previous request must not contaminate unrelated callbacks"
    );
    actor.stop();
    until(|| actor.is_quiescent(), Duration::from_secs(3));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Task), 0);
}
