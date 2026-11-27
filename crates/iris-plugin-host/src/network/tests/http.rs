use super::super::*;
use crate::{
    actor::{Executor, IoOutcome, TaskContext},
    instance::Blob,
    ledger::{Ledger, Resource},
};
use iris_api::v2::{Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role, task::Task};
use iris_base::net::http_client::HttpCancel;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

fn headers(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut buf).unwrap();
        bytes.push(buf[0]);
        assert!(bytes.len() < 65536);
    }
    String::from_utf8(bytes).unwrap()
}
fn server(
    responses: Vec<&'static [u8]>,
) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/start", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        for response in responses {
            let end = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    break stream;
                }
                assert!(Instant::now() < end, "request never arrived");
                std::thread::sleep(Duration::from_millis(5));
            };
            tx.send(headers(&mut stream)).unwrap();
            stream.write_all(response).unwrap();
        }
    });
    (url, rx, thread)
}
fn setup(url: &str) -> (NetworkGrant, NetworkExecutor, Id, Ledger) {
    let ledger = Ledger::default();
    let grant = NetworkGrant::new(
        "http-tests".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let origin = grant
        .confirm_origin(url, TransportKind::Http, &["127.0.0.1".parse().unwrap()])
        .unwrap();
    let executor = NetworkExecutor::new(grant.clone(), ledger.clone(), BTreeSet::new()).unwrap();
    (grant, executor, origin, ledger)
}
fn context(ledger: &Ledger) -> TaskContext {
    TaskContext {
        instance_id: Id(1),
        window: None,
        owner: "http-tests".into(),
        role: Role::Control,
        generation: Generation::default(),
        cancel: HttpCancel::default(),
        ledger: ledger.clone(),
    }
}
fn start(url: &str, origin: Id, follow_redirects: bool, stream_body: bool) -> Task {
    Task::HttpStart {
        method: if stream_body { "POST" } else { "GET" }.into(),
        url: url.into(),
        headers: Vec::new(),
        body_handle: None,
        stream_body,
        follow_redirects,
        timeout_ms: Some(2000),
        origin_grant: Some(origin),
    }
}
fn execute(
    executor: &NetworkExecutor,
    task: Task,
    ledger: &Ledger,
) -> Result<IoOutcome, ErrorCode> {
    run(executor.prepare(task, &BTreeMap::new(), context(ledger))?)
}
fn run(operation: crate::actor::Operation) -> Result<IoOutcome, ErrorCode> {
    let mut outcome = receive(operation)?;
    outcome.acknowledge();
    Ok(outcome)
}
fn receive(operation: crate::actor::Operation) -> Result<IoOutcome, ErrorCode> {
    let pool = crate::pool::IoPool::new()?;
    let (tx, rx) = mpsc::sync_channel(1);
    operation.dispatch(
        &pool,
        Role::Control,
        "test",
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    )?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| ErrorCode::Timeout)?
}
fn handle(outcome: &IoOutcome) -> Id {
    serde_json::from_value(outcome.value["stream_handle"].clone()).unwrap()
}

#[test]
fn discarded_http_opens_reclaim_handles_and_quota_before_retry() {
    let (url, rx, server) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndata";
        16
    ]);
    let (_grant, executor, origin, ledger) = setup(&url);
    let baseline = ledger.used(None, Resource::HostBytes);
    for attempt in 0..16 {
        let context = context(&ledger);
        let cancel = context.cancel.clone();
        let opened = receive(
            executor
                .prepare(start(&url, origin, false, false), &BTreeMap::new(), context)
                .unwrap(),
        )
        .unwrap();
        let id = handle(&opened);
        assert_eq!(ledger.used(None, Resource::ControlHttp), 1);
        // Both request cancellation and an abandoned delivery must reclaim it.
        if attempt % 2 == 0 {
            cancel.cancel();
        }
        drop(opened);
        for resource in [Resource::Http, Resource::ControlHttp, Resource::Handle] {
            assert_eq!(ledger.used(None, resource), 0);
        }
        assert_eq!(ledger.used(None, Resource::HostBytes), baseline);
        assert_eq!(executor.close(id.0), Err(ErrorCode::StaleHandle));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    server.join().unwrap();
}

#[test]
fn repeated_headers_bounded_reads_and_http_errors_are_preserved() {
    let (url,rx,thread)=server(vec![b"HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\nSet-Cookie: a=1; Path=/\r\nSet-Cookie: b=2; Path=/\r\n\r\nabcdef"]);
    let (_grant, executor, origin, ledger) = setup(&url);
    let opened = execute(&executor, start(&url, origin, false, false), &ledger).unwrap();
    assert_eq!(opened.value["status"], 403);
    assert_eq!(
        opened.value["headers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h[0] == "set-cookie")
            .count(),
        2
    );
    let handle = handle(&opened);
    drop(opened);
    let first = execute(&executor, Task::HttpRead { handle, len: 4 }, &ledger).unwrap();
    assert_eq!(first.blob.as_ref().unwrap().bytes, b"abcd");
    assert!(matches!(
        executor.prepare(
            Task::HttpRead { handle, len: 4 },
            &BTreeMap::new(),
            context(&ledger)
        ),
        Err(ErrorCode::BadState)
    ));
    drop(first); // terminal delivery releases the per-handle operation slot
    let second = execute(&executor, Task::HttpRead { handle, len: 4 }, &ledger).unwrap();
    assert_eq!(second.blob.as_ref().unwrap().bytes, b"ef");
    drop(second);
    assert_eq!(
        execute(&executor, Task::HttpRead { handle, len: 4 }, &ledger)
            .unwrap()
            .value["eof"],
        true
    );
    executor.close(handle.0).unwrap();
    assert_eq!(ledger.used(None, Resource::Http), 0);
    assert!(rx.recv().unwrap().starts_with("GET /start HTTP/1.1\r\n"));
    thread.join().unwrap();
}

#[test]
fn redirect_cookies_remain_origin_bound_and_plaintext_auth_needs_confirmation() {
    let (url,rx,thread)=server(vec![b"HTTP/1.1 302 Found\r\nLocation: /end\r\nSet-Cookie: a=1; Path=/\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]);
    let (grant, executor, origin, ledger) = setup(&url);
    grant.confirm_insecure_auth(origin).unwrap();
    let result = execute(&executor, start(&url, origin, true, false), &ledger).unwrap();
    assert_eq!(result.value["status"], 200);
    assert!(!rx.recv().unwrap().contains("Cookie:"));
    assert!(rx.recv().unwrap().contains("Cookie: a=1\r\n"));
    executor.stop();
    thread.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (_grant, executor, origin, ledger) = setup(&url);
    let mut task = start(&url, origin, false, false);
    if let Task::HttpStart { headers, .. } = &mut task {
        headers.push(("Authorization".into(), "Basic secret".into()));
    }
    assert!(matches!(
        executor.prepare(task, &BTreeMap::new(), context(&ledger)),
        Err(ErrorCode::Denied)
    ));
    assert!(listener.accept().is_err());
}

#[test]
fn stream_upload_uses_host_framing_and_returns_response_after_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/upload", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = headers(&mut stream);
        assert!(head.contains("Transfer-Encoding: chunked\r\n"));
        let mut body = Vec::new();
        let mut byte = [0; 1];
        while !body.ends_with(b"0\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            body.push(byte[0]);
            assert!(body.len() < 100);
        }
        assert_eq!(body, b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n");
        stream
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let (_grant, executor, origin, ledger) = setup(&url);
    let opened = execute(&executor, start(&url, origin, false, true), &ledger).unwrap();
    assert_eq!(opened.value["state"], "upload");
    let handle = handle(&opened);
    for (bytes, finish) in [(b"abc", false), (b"def", true)] {
        let blob = Arc::new(Blob::new(&ledger, "http-tests", bytes.to_vec()).unwrap());
        let blobs = BTreeMap::from([(9, blob)]);
        let result = run(executor
            .prepare(
                Task::HttpWrite {
                    handle,
                    blob: Id(9),
                    finish,
                },
                &blobs,
                context(&ledger),
            )
            .unwrap())
        .unwrap();
        if finish {
            assert_eq!(result.value["status"], 201);
        }
    }
    executor.stop();
    thread.join().unwrap();
}

#[test]
fn revocation_interrupts_a_blocked_socket_and_other_instances_cannot_reuse_handles() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        headers(&mut stream);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
            .unwrap();
        let _ = stream.read(&mut [0; 1]);
    });
    let (grant, executor, origin, ledger) = setup(&url);
    let opened = execute(&executor, start(&url, origin, false, false), &ledger).unwrap();
    let handle = handle(&opened);
    let mut other = context(&ledger);
    other.instance_id = Id(2);
    assert!(matches!(
        executor.prepare(Task::HttpRead { handle, len: 10 }, &BTreeMap::new(), other),
        Err(ErrorCode::Denied)
    ));
    let read = executor
        .prepare(
            Task::HttpRead { handle, len: 10 },
            &BTreeMap::new(),
            context(&ledger),
        )
        .unwrap();
    let worker = std::thread::spawn(move || run(read));
    std::thread::sleep(Duration::from_millis(20));
    let start = Instant::now();
    grant.revoke_origin(origin).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    executor.stop();
    server.join().unwrap();
    assert_eq!(ledger.used(None, Resource::Http), 0);
}
