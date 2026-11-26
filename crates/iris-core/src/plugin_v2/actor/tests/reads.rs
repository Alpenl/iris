use super::*;

#[test]
fn completed_reads_release_delivery_state_without_a_seek() {
    let shared = mailbox();
    for n in 0..2000 {
        let ticket = shared.read("resource".into(), n, 1, None).unwrap();
        let id = Id(ticket.request);
        let event = shared.next().unwrap();
        assert!(matches!(event, Message::Event { read: Some(read), .. } if read == id));
        assert!(shared.deliver_read(id));
        drop(event);
        if n % 2 == 0 {
            shared.reply(id.0, blob(&shared, b"x"), false).unwrap();
            let result = ticket.receive(Duration::from_secs(1)).unwrap();
            assert_eq!(result.blob.bytes, b"x");
        } else {
            shared.fail(id.0, ErrorCode::IoError).unwrap();
            assert!(matches!(
                ticket.receive(Duration::from_secs(1)),
                Err(ErrorCode::IoError)
            ));
        }
        assert!(!shared.has_read(id));
        assert!(
            !shared.deliver_read(id),
            "terminal requests must retain no delivery record"
        );
        drop(ticket);
        assert_eq!(shared.ledger.used(None, Resource::StreamRead), 0);
        assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
    }
    // Dropping completed tickets must not enqueue cancellation notifications.
    shared.send(b"{}".to_vec(), None).unwrap();
    assert!(matches!(
        shared.next(),
        Some(Message::Event { read: None, .. })
    ));
    shared.stop();
}

#[test]
fn cancellation_remembers_delivery_only_for_its_own_live_request() {
    for delivered in [false, true] {
        let shared = mailbox();
        let ticket = shared.read("resource".into(), 0, 1, None).unwrap();
        let id = Id(ticket.request);
        drop(shared.next().unwrap());
        if delivered {
            assert!(shared.deliver_read(id));
        }
        drop(ticket);
        assert!(!shared.deliver_read(id));
        let notice = shared.next().unwrap();
        assert!(matches!(&notice, Message::Cancelled { request, read }
            if *request == id && read.delivered == delivered));
        // Cancellation delivery continues to own its bounded read slot.
        assert_eq!(shared.ledger.used(None, Resource::StreamRead), 1);
        drop(notice);
        assert_eq!(shared.ledger.used(None, Resource::StreamRead), 0);
        assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
        shared.stop();
    }
}

#[test]
fn seek_releases_delivered_reads_and_does_not_mark_the_next_request() {
    let shared = mailbox();
    let old = shared.read("resource".into(), 0, 1, None).unwrap();
    drop(shared.next().unwrap());
    assert!(shared.deliver_read(Id(old.request)));
    shared
        .advance(Generation {
            seek: Id(1),
            ..Generation::default()
        })
        .unwrap();
    assert!(matches!(
        old.receive(Duration::from_secs(1)),
        Err(ErrorCode::Cancelled)
    ));
    assert!(!shared.deliver_read(Id(old.request)));
    assert!(matches!(shared.next(), Some(Message::Sweep)));
    let next = shared.read("resource".into(), 0, 1, None).unwrap();
    drop(shared.next().unwrap());
    drop(next);
    assert!(matches!(shared.next(), Some(Message::Cancelled { read, .. }) if !read.delivered));
    assert_eq!(shared.ledger.used(None, Resource::HostBytes), 0);
    shared.stop();
}

#[test]
fn receive_cancellable_wakes_immediately_on_http_cancel() {
    let shared = mailbox();
    let ticket = shared.read("resource".into(), 0, 1, None).unwrap();
    let cancel = HttpCancel::default();
    let worker_cancel = cancel.clone();
    let starter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        worker_cancel.cancel();
    });
    let started = Instant::now();
    assert!(matches!(
        ticket.receive_cancellable(&cancel, Duration::from_secs(10)),
        Err(ErrorCode::Cancelled)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "receive_cancellable must wake via Condvar callback rather than waiting on timeout"
    );
    starter.join().unwrap();
    drop(ticket);
    shared.stop();
}
