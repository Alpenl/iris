use super::super::*;
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        instance::Blob,
        ledger::{Ledger, Resource},
        pool::IoPool,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostRule, Id, NetworkDeclaration, Role, task::Task,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

fn context(ledger: &Ledger) -> TaskContext {
    TaskContext {
        instance_id: Id(10),
        window: None,
        owner: "socket-tests".into(),
        role: Role::Control,
        generation: Generation::default(),
        cancel: HttpCancel::default(),
        ledger: ledger.clone(),
    }
}
fn setup(port: u16, ws: bool) -> (NetworkGrant, NetworkExecutor, Id, Ledger) {
    let ledger = Ledger::default();
    let declaration = if ws {
        NetworkDeclaration::default()
    } else {
        NetworkDeclaration {
            tcp_targets: vec![HostRule {
                host: "127.0.0.1".into(),
                ports: vec![port],
                schemes: vec!["tcp".into()],
            }],
            ..Default::default()
        }
    };
    let grant = NetworkGrant::new(
        "socket-tests".into(),
        BTreeSet::from([Capability::NetTcp, Capability::NetWs]),
        declaration,
        &ledger,
    )
    .unwrap();
    let origin = grant
        .confirm_origin(
            &format!("{}://127.0.0.1:{port}/", if ws { "ws" } else { "http" }),
            if ws {
                TransportKind::WebSocket
            } else {
                TransportKind::Tcp { tls: false }
            },
            &["127.0.0.1".parse().unwrap()],
        )
        .unwrap();
    let executor = NetworkExecutor::new(grant.clone(), ledger.clone(), BTreeSet::new()).unwrap();
    (grant, executor, origin, ledger)
}
fn dispatch(pool: &IoPool, operation: Operation) -> mpsc::Receiver<Result<IoOutcome, ErrorCode>> {
    let (tx, rx) = mpsc::channel();
    operation
        .dispatch(
            pool,
            Role::Control,
            "socket-tests",
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        )
        .unwrap();
    rx
}
fn wait(rx: mpsc::Receiver<Result<IoOutcome, ErrorCode>>) -> IoOutcome {
    rx.recv_timeout(Duration::from_secs(4)).unwrap().unwrap()
}
fn run(pool: &IoPool, executor: &NetworkExecutor, task: Task, ledger: &Ledger) -> IoOutcome {
    wait(dispatch(
        pool,
        executor
            .prepare(task, &BTreeMap::new(), context(ledger))
            .unwrap(),
    ))
}
fn id(result: &IoOutcome) -> Id {
    serde_json::from_value(result.value["stream_handle"].clone()).unwrap()
}
fn released(ledger: &Ledger, resource: Resource) {
    let end = Instant::now() + Duration::from_secs(2);
    while ledger.used(None, resource) > 0 {
        assert!(Instant::now() < end, "socket resource not reclaimed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn pending_tcp_reads_do_not_block_writes_and_large_writes_are_not_truncated() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut workers = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            workers.push(std::thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = vec![0; 1 << 20];
                stream.read_exact(&mut bytes).unwrap();
                assert!(bytes.iter().all(|b| *b == 42));
                stream.write_all(b"ack").unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let (_grant, executor, origin, ledger) = setup(port, false);
    let pool = IoPool::new().unwrap();
    let mut handles = Vec::new();
    let mut reads = Vec::new();
    for _ in 0..2 {
        let handle = id(&run(
            &pool,
            &executor,
            Task::TcpConnect {
                host: "127.0.0.1".into(),
                port,
                tls: false,
                origin_grant: Some(origin),
            },
            &ledger,
        ));
        handles.push(handle);
        reads.push(dispatch(
            &pool,
            executor
                .prepare(
                    Task::TcpRead { handle, len: 3 },
                    &BTreeMap::new(),
                    context(&ledger),
                )
                .unwrap(),
        ));
    }
    let blob = Arc::new(Blob::new(&ledger, "socket-tests", vec![42; 1 << 20]).unwrap());
    let blobs = BTreeMap::from([(1, blob)]);
    for handle in &handles {
        let written = wait(dispatch(
            &pool,
            executor
                .prepare(
                    Task::TcpWrite {
                        handle: *handle,
                        blob: Id(1),
                    },
                    &blobs,
                    context(&ledger),
                )
                .unwrap(),
        ));
        assert_eq!(written.value["written"], "1048576");
    }
    for read in reads {
        assert_eq!(wait(read).blob.as_ref().unwrap().bytes, b"ack");
    }
    for handle in handles {
        executor.close(handle.0).unwrap();
    }
    server.join().unwrap();
    released(&ledger, Resource::Tcp);
}

#[test]
fn websocket_handles_ping_and_full_duplex_without_blocking_the_store() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        socket
            .send(tungstenite::Message::Ping(b"ping".to_vec().into()))
            .unwrap();
        loop {
            let message = socket.read().unwrap();
            if let tungstenite::Message::Text(text) = message {
                assert_eq!(text, "hello");
                break;
            }
        }
        socket
            .send(tungstenite::Message::Binary(b"reply".to_vec().into()))
            .unwrap();
        loop {
            match socket.read() {
                Err(_) | Ok(tungstenite::Message::Close(_)) => break,
                _ => {}
            }
        }
    });
    let (grant, executor, origin, ledger) = setup(port, true);
    let pool = IoPool::new().unwrap();
    let handle = id(&run(
        &pool,
        &executor,
        Task::WsConnect {
            url: format!("ws://127.0.0.1:{port}/socket"),
            headers: Vec::new(),
            origin_grant: Some(origin),
        },
        &ledger,
    ));
    let read = dispatch(
        &pool,
        executor
            .prepare(Task::WsRead { handle }, &BTreeMap::new(), context(&ledger))
            .unwrap(),
    );
    let blobs = BTreeMap::from([(
        1,
        Arc::new(Blob::new(&ledger, "socket-tests", b"hello".to_vec()).unwrap()),
    )]);
    let written = wait(dispatch(
        &pool,
        executor
            .prepare(
                Task::WsWrite {
                    handle,
                    blob: Id(1),
                    text: true,
                },
                &blobs,
                context(&ledger),
            )
            .unwrap(),
    ));
    assert_eq!(written.value["written"], "5");
    drop(written);
    let result = wait(read);
    assert_eq!(result.blob.as_ref().unwrap().bytes, b"reply");
    assert_eq!(result.value["text"], false);
    drop(result);
    let pending = dispatch(
        &pool,
        executor
            .prepare(Task::WsRead { handle }, &BTreeMap::new(), context(&ledger))
            .unwrap(),
    );
    grant.revoke_origin(origin).unwrap();
    assert!(
        pending
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err()
    );
    executor.close(handle.0).unwrap();
    server.join().unwrap();
    released(&ledger, Resource::WebSocket);
}

#[test]
fn websocket_rejects_oversized_frame_before_allocating_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        let mut header = vec![0x82, 127];
        header.extend_from_slice(&(2u64 << 20).to_be_bytes());
        socket.get_mut().write_all(&header).unwrap();
        let _ = socket.get_mut().read(&mut [0; 1]);
    });
    let (_grant, executor, origin, ledger) = setup(port, true);
    let pool = IoPool::new().unwrap();
    let handle = id(&run(
        &pool,
        &executor,
        Task::WsConnect {
            url: format!("ws://127.0.0.1:{port}/"),
            headers: Vec::new(),
            origin_grant: Some(origin),
        },
        &ledger,
    ));
    // The reactor may already have rejected the header before the guest queues its read.
    match executor.prepare(Task::WsRead { handle }, &BTreeMap::new(), context(&ledger)) {
        Ok(operation) => assert!(
            dispatch(&pool, operation)
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        ),
        Err(error) => assert!(matches!(
            error,
            ErrorCode::Cancelled | ErrorCode::QuotaExceeded
        )),
    }
    executor.close(handle.0).unwrap();
    server.join().unwrap();
    released(&ledger, Resource::WebSocket);
}
