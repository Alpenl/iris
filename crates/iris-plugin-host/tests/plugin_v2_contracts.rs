//! Run with scripts/test-plugin-v2.py, which builds both real guest languages.
use iris_api::v2::{Capability, ErrorCode, Generation, HostCommand, Reply, Role, task::Task};
use iris_base::rt::CancelToken;
use iris_plugin_host::{
    actor::{Actor, Executor, IoOutcome, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    instance::{Blob, Instance, InstanceConfig},
    ledger::{Ledger, Resource},
    pool::IoPool,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
#[path = "plugin_v2_contracts/runtime.rs"]
mod runtime;
#[path = "plugin_v2_contracts/wat_abi.rs"]
mod wat_abi;
#[path = "plugin_v2_contracts/windows.rs"]
mod windows;

fn guest(name: &str) -> Vec<u8> {
    std::fs::read(
        std::env::var(name).expect("run scripts/test-plugin-v2.py to compile language fixtures"),
    )
    .unwrap()
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

fn until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let end = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
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
    stream.send_context(serde_json::json!({"event":"stream_init","session":"1","generation":Generation::default()}), context).unwrap();
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
    use iris_api::v2::{Id, NetworkDeclaration, stream::StreamContext};
    use iris_base::net::http_client::HttpCancel;
    use iris_plugin_host::{
        network::NetworkGrant,
        stream::{
            guest::StreamStore,
            relay::BoundRelay,
            remote::RemoteFactory,
            session::{StreamSession, new_session_id},
        },
    };
    use std::{
        io::{Read, Write},
        net::TcpStream,
    };
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
