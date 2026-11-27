use super::*;

#[test]
fn single_window_advance_close_and_reopen_enforce_exclusivity() {
    let shared = Mailbox::new(
        "windows".into(),
        Role::Control,
        Generation::default(),
        BTreeSet::new(),
        Ledger::default(),
        Arc::new(Reject),
        Arc::new(|| {}),
    )
    .unwrap();
    let first = Generation {
        media: Id(1),
        seek: Id(5),
        ..Generation::default()
    };
    shared.open_window(Id(1), first).unwrap();
    assert_eq!(
        shared.open_window(Id(2), first),
        Err(ErrorCode::QuotaExceeded)
    );
    assert_eq!(shared.open_window(Id(1), first), Err(ErrorCode::BadState));
    let a = shared.io_context(None, Some(Id(1))).unwrap();
    let global = shared.io_context(None, None).unwrap();
    let command = br#"{"event":"command","id":"test","snapshot":null}"#.to_vec();
    shared.send_window(Id(1), first, command.clone()).unwrap();
    let next = Generation {
        media: Id(2),
        ..Generation::default()
    };
    shared.advance_window(Id(1), next).unwrap();
    assert!(a.cancel.is_cancelled());
    assert!(!global.cancel.is_cancelled());
    assert!(!shared.is_current(Some(Id(1)), first));
    assert!(shared.is_current(Some(Id(1)), next));
    assert!(matches!(shared.next(), Some(Message::Sweep)));
    assert_eq!(
        shared.send_window(Id(1), first, command.clone()),
        Err(ErrorCode::StaleHandle)
    );
    assert_eq!(
        shared.advance_window(Id(1), first),
        Err(ErrorCode::StaleHandle)
    );
    let current = shared.io_context(None, Some(Id(1))).unwrap();
    shared.close_window(Id(1)).unwrap();
    assert!(current.cancel.is_cancelled());
    assert!(!global.cancel.is_cancelled());
    assert!(matches!(shared.next(), Some(Message::Sweep)));
    assert!(matches!(
        shared.next(),
        Some(Message::Closed { window: Id(1), .. })
    ));
    assert_eq!(
        shared.send_window(Id(1), next, command.clone()),
        Err(ErrorCode::StaleHandle)
    );

    shared.open_window(Id(2), first).unwrap();
    let b = shared.io_context(None, Some(Id(2))).unwrap();
    shared.send_window(Id(2), first, command).unwrap();
    assert!(matches!(
        shared.next(),
        Some(Message::Event {
            window: Some(Id(2)),
            ..
        })
    ));
    shared.stop();
    assert!(global.cancel.is_cancelled());
    assert!(b.cancel.is_cancelled());
    assert_eq!(shared.ledger.used(None, Resource::Handle), 0);
    assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn operation_cancellation_cannot_cross_window_scope() {
    let shared = Arc::new(
        Mailbox::new(
            "windows".into(),
            Role::Control,
            Generation::default(),
            BTreeSet::new(),
            Ledger::default(),
            Arc::new(Reject),
            Arc::new(|| {}),
        )
        .unwrap(),
    );
    let cancel = HttpCancel::default();
    shared.operations.lock().unwrap().insert(
        10,
        PendingOperation {
            cancel: cancel.clone(),
            window: Some(Id(1)),
        },
    );
    let mut bridge = Bridge {
        shared: shared.clone(),
        io: IoPool::new().unwrap(),
        generation: Generation::default(),
        read: None,
        window: Some(Id(2)),
        request_cancel: None,
    };
    assert_eq!(bridge.close(10), Err(ErrorCode::StaleHandle));
    assert!(!cancel.is_cancelled());
    bridge.on_window(Some(Id(1)));
    bridge.close(10).unwrap();
    assert!(cancel.is_cancelled());
    shared.stop();
}

#[test]
fn window_closed_is_delivered_even_after_global_generation_advances() {
    // 1. Deterministic mailbox check: close queued at gen0 survives advance to media=5.
    let shared = Mailbox::new(
        "closed-mailbox".into(),
        Role::Control,
        Generation::default(),
        BTreeSet::new(),
        Ledger::default(),
        Arc::new(Reject),
        Arc::new(|| {}),
    )
    .unwrap();
    shared.open_window(Id(1), Generation::default()).unwrap();
    shared.close_window(Id(1)).unwrap();
    shared
        .advance(Generation {
            media: Id(5),
            ..Generation::default()
        })
        .unwrap();
    assert!(matches!(shared.next(), Some(Message::Sweep)));
    assert!(matches!(
        shared.next(),
        Some(Message::Closed { window: Id(1), .. })
    ));
    shared.stop();

    // 2. End-to-end Actor check: guest records window_closed even when global generation
    //    advances while window_closed is queued behind an in-flight event.
    let reply = r#"{"ok":[{"command":"show_toast","message":"closed"}]}"#;
    let data = reply
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let wasm = wat::parse_str(format!(
        r#"(module
        (import "iris_v2" "host_clock" (func $clock (param i32) (result i64)))
        (memory (export "memory") 4 8)
        (global $saw_closed (mut i32) (i32.const 0))
        (data (i32.const 2048) "{data}")
        (data (i32.const 4096) "\7b\22ok\22:[]\7d")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        (func $contains4 (param $ptr i32) (param $len i32) (param $needle i32) (result i32)
            (local $i i32)
            (block $end (loop $scan
                local.get $i i32.const 4 i32.add local.get $len i32.gt_u br_if $end
                local.get $ptr local.get $i i32.add i32.load align=1 local.get $needle i32.eq
                if i32.const 1 return end
                local.get $i i32.const 1 i32.add local.set $i br $scan))
            i32.const 0)
        (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
            (local $until i64)
            local.get $ptr local.get $len i32.const {spin} call $contains4
            if
                i32.const 1 call $clock i64.const 40 i64.add local.set $until
                (loop $wait
                    i32.const 1 call $clock local.get $until i64.lt_s br_if $wait)
            end
            local.get $ptr local.get $len i32.const {closed} call $contains4
            if
                i32.const 1 global.set $saw_closed
            end
            local.get $ptr local.get $len i32.const {probe} call $contains4
            global.get $saw_closed i32.and
            if (result i64) i64.const {} else i64.const {} end))"#,
        (2048_u64 << 32) | reply.len() as u64,
        (4096_u64 << 32) | 9,
        spin = i32::from_le_bytes(*b"spin"),
        closed = i32::from_le_bytes(*b"clos"),
        probe = i32::from_le_bytes(*b"prob"),
    ))
    .unwrap();

    let engine = RuntimeEngine::new().unwrap();
    let module = engine
        .compile(&wasm, &iris_base::rt::CancelToken::new())
        .unwrap();
    let ledger = Ledger::default();
    let actor = Actor::spawn(Spawn {
        engine,
        module,
        role: Role::Control,
        owner: "closed-gen".into(),
        grants: BTreeSet::from([Capability::Notifications]),
        ledger,
        io: IoPool::new().unwrap(),
        executor: Arc::new(Reject),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    actor.send(br#"{"event":"init"}"#.to_vec()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !actor.initialized() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    actor.open_window(Id(1), Generation::default()).unwrap();
    // Keep the actor busy while close_window is queued and global generation advances.
    actor.send(br#"{"event":"spin"}"#.to_vec()).unwrap();
    actor.close_window(Id(1)).unwrap();
    actor
        .advance(Generation {
            media: Id(5),
            ..Generation::default()
        })
        .unwrap();
    actor.send(br#"{"event":"probe"}"#.to_vec()).unwrap();
    let mut saw_closed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !saw_closed && std::time::Instant::now() < deadline {
        if let Some(update) = actor.poll()
            && update.commands.iter().any(|c| {
                matches!(
                    c,
                    HostCommand::ShowToast { message } if message == "closed"
                )
            })
        {
            saw_closed = true;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        saw_closed,
        "terminal window_closed must not be dropped when global generation advances"
    );
    actor.stop();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !actor.is_quiescent() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn concurrent_close_during_open_or_advance_emits_compensating_window_close_to_executor() {
    struct HookExecutor {
        entered: mpsc::SyncSender<()>,
        proceed: Mutex<mpsc::Receiver<()>>,
        pause_next_some: AtomicBool,
        states: Mutex<BTreeMap<Id, Option<Generation>>>,
        events: Mutex<Vec<(Id, Option<Generation>)>>,
    }
    impl Executor for HookExecutor {
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
        fn window_changed(&self, window: Id, generation: Option<Generation>) {
            if generation.is_some() && self.pause_next_some.swap(false, Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.proceed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            self.states.lock().unwrap().insert(window, generation);
            self.events.lock().unwrap().push((window, generation));
        }
        fn stop(&self) {}
    }

    // 1. close_window racing open_window while executor.window_changed(Some) is in flight.
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (proceed_tx, proceed_rx) = mpsc::sync_channel(1);
    let exec = Arc::new(HookExecutor {
        entered: entered_tx,
        proceed: Mutex::new(proceed_rx),
        pause_next_some: AtomicBool::new(true),
        states: Mutex::new(BTreeMap::new()),
        events: Mutex::new(Vec::new()),
    });
    let shared = Arc::new(
        Mailbox::new(
            "window-race".into(),
            Role::Control,
            Generation::default(),
            BTreeSet::new(),
            Ledger::default(),
            exec.clone(),
            Arc::new(|| {}),
        )
        .unwrap(),
    );
    let gen0 = Generation {
        media: Id(1),
        ..Generation::default()
    };
    let opener_shared = shared.clone();
    let opener = std::thread::spawn(move || opener_shared.open_window(Id(1), gen0));
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    shared.close_window(Id(1)).unwrap();
    proceed_tx.send(()).unwrap();
    assert_eq!(opener.join().unwrap(), Err(ErrorCode::StaleHandle));
    assert_eq!(exec.states.lock().unwrap().get(&Id(1)), Some(&None));
    assert_eq!(
        exec.events.lock().unwrap().last(),
        Some(&(Id(1), None)),
        "compensating None must clear zombie Some(gen0) state"
    );

    // 2. close_window racing advance_window while executor.window_changed(Some) is in flight.
    shared.open_window(Id(2), gen0).unwrap();
    assert_eq!(exec.states.lock().unwrap().get(&Id(2)), Some(&Some(gen0)));
    exec.pause_next_some.store(true, Ordering::Release);
    let gen1 = Generation {
        media: Id(2),
        ..Generation::default()
    };
    let advancer_shared = shared.clone();
    let advancer = std::thread::spawn(move || advancer_shared.advance_window(Id(2), gen1));
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    shared.close_window(Id(2)).unwrap();
    proceed_tx.send(()).unwrap();
    assert_eq!(advancer.join().unwrap(), Err(ErrorCode::StaleHandle));
    assert_eq!(exec.states.lock().unwrap().get(&Id(2)), Some(&None));
    assert_eq!(exec.events.lock().unwrap().last(), Some(&(Id(2), None)));
    shared.stop();
}

#[test]
fn open_window_with_event_rolls_back_under_backpressure_without_queuing_window_closed() {
    let shared = Mailbox::new(
        "open-rollback".into(),
        Role::Control,
        Generation::default(),
        BTreeSet::new(),
        Ledger::default(),
        Arc::new(Reject),
        Arc::new(|| {}),
    )
    .unwrap();
    let gen0 = Generation::default();
    let ping = br#"{"event":"command","id":"fill","snapshot":null}"#.to_vec();
    let opened = br#"{"event":"window_opened","snapshot":null}"#.to_vec();

    for _ in 0..32 {
        shared.send(ping.clone(), None).unwrap();
    }

    // 10 failed retries with the same window ID must return Backpressure without
    // pushing phantom Message::Closed entries or exhausting the 8-slot close queue.
    for _ in 0..10 {
        assert_eq!(
            shared.open_window_with_event(Id(9), gen0, opened.clone()),
            Err(ErrorCode::Backpressure)
        );
    }
    assert!(!shared.is_current(Some(Id(9)), gen0));

    // Drain one event so space opens up; retry with the exact same Id(9) succeeds cleanly.
    assert!(matches!(
        shared.next(),
        Some(Message::Event { window: None, .. })
    ));
    shared.open_window_with_event(Id(9), gen0, opened).unwrap();
    assert!(shared.is_current(Some(Id(9)), gen0));

    // None of the queued messages may be a phantom Message::Closed for Id(9).
    for _ in 0..32 {
        let msg = shared.next().unwrap();
        assert!(!matches!(msg, Message::Closed { .. }));
    }
    shared.stop();
    assert_eq!(shared.ledger.used(None, Resource::Handle), 0);
    assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
}
