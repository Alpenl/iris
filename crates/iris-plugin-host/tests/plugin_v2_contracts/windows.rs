use super::*;
use iris_api::v2::Id;
use iris_plugin_host::actor::Completion;
use std::sync::Mutex;

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
    // Cancelled work remains charged until its actual terminal callback.
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

type PendingRequests = Arc<Mutex<Vec<(iris_core::vfs::HttpCancel, Completion)>>>;
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
    use iris_core::vfs::HttpCancel;
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
