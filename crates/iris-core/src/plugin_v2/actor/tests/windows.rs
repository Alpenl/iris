use super::*;

#[test]
fn seek_and_close_cancel_only_the_selected_window() {
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
    shared.open_window(Id(2), first).unwrap();
    let a = shared.io_context(None, Some(Id(1))).unwrap();
    let b = shared.io_context(None, Some(Id(2))).unwrap();
    let global = shared.io_context(None, None).unwrap();
    let command = br#"{"event":"command","id":"test","snapshot":null}"#.to_vec();
    shared.send_window(Id(1), first, command.clone()).unwrap();
    shared.send_window(Id(2), first, command.clone()).unwrap();
    let next = Generation {
        media: Id(2),
        ..Generation::default()
    };
    shared.advance_window(Id(1), next).unwrap();
    assert!(a.cancel.is_cancelled());
    assert!(!b.cancel.is_cancelled());
    assert!(!global.cancel.is_cancelled());
    assert!(!shared.is_current(Some(Id(1)), first));
    assert!(shared.is_current(Some(Id(2)), first));
    assert!(matches!(shared.next(), Some(Message::Sweep)));
    assert!(matches!(
        shared.next(),
        Some(Message::Event {
            window: Some(Id(2)),
            ..
        })
    ));
    assert_eq!(
        shared.send_window(Id(1), first, command.clone()),
        Err(ErrorCode::StaleHandle)
    );
    assert_eq!(
        shared.advance_window(Id(1), first),
        Err(ErrorCode::StaleHandle)
    );
    let current = shared.io_context(None, Some(Id(1))).unwrap();
    shared.close_window(Id(2)).unwrap();
    assert!(b.cancel.is_cancelled());
    assert!(!current.cancel.is_cancelled());
    assert_eq!(
        shared.send_window(Id(2), first, command),
        Err(ErrorCode::StaleHandle)
    );
    shared.stop();
    assert!(global.cancel.is_cancelled());
    assert!(current.cancel.is_cancelled());
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
