use super::*;
use crate::{
    actor::ReadResult,
    instance::Blob,
    ledger::Resource,
    stream::{
        reader::{MemoryReader, OpenedStream, Reader, StreamSlice},
        relay::BoundRelay,
    },
};
use iris_api::v2::{ErrorCode, stream::ManifestFormat};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::budget;
use serde_json::json;
use std::{
    io::{BufRead, BufReader},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn decode_chunked(raw_body: &[u8]) -> Vec<u8> {
    let mut reader = BufReader::new(raw_body);
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "unexpected EOF before terminal 0 chunk");
        assert!(
            line.ends_with("\r\n"),
            "chunk header missing CRLF: {line:?}"
        );
        let size = usize::from_str_radix(line.trim_end_matches("\r\n"), 16).unwrap();
        if size == 0 {
            let mut end = [0u8; 2];
            reader.read_exact(&mut end).unwrap();
            assert_eq!(&end, b"\r\n");
            break;
        }
        let start = out.len();
        out.resize(start + size, 0);
        reader.read_exact(&mut out[start..]).unwrap();
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).unwrap();
        assert_eq!(&crlf, b"\r\n");
    }
    out
}

struct StreamingReader {
    data: Arc<Vec<u8>>,
    known_size: bool,
    use_open_stream: bool,
    ledger: Ledger,
}

struct VecRest {
    data: Arc<Vec<u8>>,
    pos: usize,
    end: usize,
    cancel: HttpCancel,
}

impl StreamSlice for VecRest {
    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if self.pos >= self.end {
            return Ok(0);
        }
        let n = buf.len().min(64 << 10).min(self.end - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Reader for StreamingReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(self.known_size.then_some(self.data.len() as u64))
    }

    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let start = usize::try_from(offset).map_err(|_| ErrorCode::RangeInvalid)?;
        if start > self.data.len() {
            return Err(ErrorCode::RangeInvalid);
        }
        let take = (len as usize)
            .min(256 << 10)
            .min(self.data.len().saturating_sub(start));
        let end = start + take;
        let blob = Blob::new(&self.ledger, "stream-tests", self.data[start..end].to_vec())?;
        Ok(ReadResult {
            blob: Arc::new(blob),
            eof: end == self.data.len(),
        })
    }

    fn open_stream(
        &self,
        offset: u64,
        len: Option<u64>,
        cancel: &HttpCancel,
    ) -> Result<Option<OpenedStream>, ErrorCode> {
        if !self.use_open_stream {
            return Ok(None);
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let start = usize::try_from(offset).map_err(|_| ErrorCode::RangeInvalid)?;
        if start > self.data.len() {
            return Err(ErrorCode::RangeInvalid);
        }
        let avail = self.data.len() - start;
        let total_take = len.map_or(avail, |n| avail.min(n as usize));
        let first_len = total_take.min(16 << 10);
        let first_end = start + first_len;
        let stream_end = start + total_take;
        Ok(Some(OpenedStream {
            first: self.data[start..first_end].to_vec(),
            total: self.known_size.then_some(self.data.len() as u64),
            remaining: self.known_size.then_some(total_take as u64),
            eof: first_end == self.data.len(),
            rest: Box::new(VecRest {
                data: Arc::clone(&self.data),
                pos: first_end,
                end: stream_end,
                cancel: cancel.clone(),
            }),
        }))
    }
}

#[test]
fn body_writes_survive_pause_longer_than_pre_body_timeout_on_read_at_and_open_stream() {
    // 24 MiB exceeds both Linux and Windows Winsock auto-tuned loopback TCP buffers,
    // forcing nonblocking body writes to hit `WouldBlock` across the client pause.
    let total = 24usize << 20;
    let payload = Arc::new(
        (0..total)
            .map(|i| ((i.wrapping_mul(41) ^ (i >> 8) ^ (i >> 16)) & 0xff) as u8)
            .collect::<Vec<u8>>(),
    );

    // Exercise both `open_stream` (`write_stream_body`, sized & unsized) and `read_at` (`write_body`).
    for (use_open_stream, known_size) in [(true, true), (true, false), (false, true)] {
        let (factory, grant, ledger) = factory();
        let bound = BoundRelay::bind().unwrap();
        let sess = session(factory, bound.authority().unwrap());
        let reader: Arc<dyn Reader> = Arc::new(StreamingReader {
            data: Arc::clone(&payload),
            known_size,
            use_open_stream,
            ledger: ledger.clone(),
        });
        sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
        let pre_body_timeout = Duration::from_millis(150);
        let relay = bound
            .start_with_timeout(sess.clone(), pre_body_timeout)
            .unwrap();
        let url = url::Url::parse(&sess.root_url().unwrap()).unwrap();

        let mut client = TcpStream::connect(sess.authority()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            url.path(),
            sess.authority()
        )
        .unwrap();

        // Pause 3x longer than `pre_body_timeout` while the relay worker fills
        // the loopback TCP buffer in nonblocking mode with SO_SNDTIMEO == None.
        std::thread::sleep(Duration::from_millis(450));

        let mut raw = Vec::with_capacity(total + 16384);
        let mut buf = vec![0u8; 64 * 1024];
        let mut reads = 0usize;
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    reads += 1;
                    if reads.is_multiple_of(32) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(err) => panic!(
                    "slow read failed (open_stream={use_open_stream}, known={known_size}): {err}"
                ),
            }
        }

        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&raw[..split]).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        if known_size {
            assert!(
                head.contains(&format!("Content-Length: {total}\r\n")),
                "{head}"
            );
            assert_eq!(&raw[split..], payload.as_slice());
        } else {
            assert!(head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
            assert_eq!(decode_chunked(&raw[split..]), *payload);
        }

        relay.stop();
        until(|| relay.is_quiescent());
        drop((client, relay, sess, grant));
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
    }
}

#[test]
fn pre_body_stalled_request_times_out_and_releases_worker_and_ledger_bytes() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    let reader = Arc::new(MemoryReader::new(b"healthy".to_vec(), "stream-tests", &ledger).unwrap());
    sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
    let relay = bound
        .start_with_timeout(sess.clone(), Duration::from_millis(150))
        .unwrap();
    let root = sess.root_url().unwrap();
    let url = url::Url::parse(&root).unwrap();

    // Connect and stall before completing `\r\n\r\n`.
    let start = Instant::now();
    let mut stalled = TcpStream::connect(sess.authority()).unwrap();
    stalled
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stalled,
        "GET {} HTTP/1.1\r\nHost: {}\r\n",
        url.path(),
        sess.authority()
    )
    .unwrap();
    let mut reply = String::new();
    stalled.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 502 Error\r\n"), "{reply:?}");
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(stalled);

    until(|| ledger.used(None, Resource::RelayWorker) == 0);

    // Subsequent valid request succeeds cleanly.
    let ok = request(&root, "GET", "");
    assert!(ok.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(ok.ends_with(b"\r\n\r\nhealthy"));

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

struct GatedReader {
    entered: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    ledger: Ledger,
}

impl Reader for GatedReader {
    fn size(&self, _: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        Ok(Some(4))
    }

    fn read_at(&self, _: u64, _: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        let (lock, cv) = &*self.gate;
        let mut open = lock.lock().unwrap();
        while !*open && !cancel.is_cancelled() {
            let (next, _) = cv.wait_timeout(open, Duration::from_millis(20)).unwrap();
            open = next;
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let blob = Blob::new(&self.ledger, "stream-tests", b"done".to_vec())?;
        Ok(ReadResult {
            blob: Arc::new(blob),
            eof: true,
        })
    }
}

#[test]
fn worker_saturation_returns_immediate_503_busy_without_blocking_listener() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    sess.commit(
        sess.prepare_reader(Arc::new(GatedReader {
            entered: Arc::clone(&entered),
            gate: Arc::clone(&gate),
            ledger: ledger.clone(),
        }))
        .unwrap(),
    )
    .unwrap();
    let relay = bound.start(sess.clone()).unwrap();
    let root = sess.root_url().unwrap();

    let cap = budget::PLUGIN_RELAY_WORKERS_PER_RELAY;
    let holders: Vec<_> = (0..cap)
        .map(|_| {
            let u = root.clone();
            std::thread::spawn(move || request(&u, "GET", ""))
        })
        .collect();
    until(|| entered.load(Ordering::Acquire) == cap);
    assert_eq!(ledger.used(None, Resource::RelayWorker), cap as u64);

    // Extra concurrent requests must receive 503 Busy immediately (no 1s queue).
    // Note: listener rejects at capacity without draining unread client request bytes,
    // which may emit a trailing TCP RST after the complete 503 header block.
    let request_busy = |raw_url: &str| -> Vec<u8> {
        let parsed = url::Url::parse(raw_url).unwrap();
        let authority = format!("{}:{}", parsed.host_str().unwrap(), parsed.port().unwrap());
        let mut sock = TcpStream::connect(&authority).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let _ = write!(
            sock,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
            parsed.path()
        );
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::ConnectionReset && !out.is_empty() =>
                {
                    break;
                }
                Err(err) => panic!("503 Busy read failed: {err}"),
            }
        }
        out
    };
    for _ in 0..2 {
        let t0 = Instant::now();
        let busy = request_busy(&root);
        assert!(
            busy.starts_with(b"HTTP/1.1 503 Busy\r\n"),
            "expected immediate 503 Busy, got {:?}",
            String::from_utf8_lossy(&busy)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "503 Busy took {:?}",
            t0.elapsed()
        );
    }

    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    for h in holders {
        let out = h.join().unwrap();
        assert!(out.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(out.ends_with(b"\r\n\r\ndone"));
    }
    until(|| ledger.used(None, Resource::RelayWorker) == 0);

    let after = request(&root, "GET", "");
    assert!(after.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(after.ends_with(b"\r\n\r\ndone"));

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

struct BlockingTailRest {
    blocked: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    cancel: HttpCancel,
}

impl StreamSlice for BlockingTailRest {
    fn read_chunk(&mut self, _: &mut [u8]) -> Result<usize, ErrorCode> {
        self.blocked.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.cancel.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(3));
        }
        if self.cancel.is_cancelled() {
            self.cancelled.store(true, Ordering::Release);
            Err(ErrorCode::Cancelled)
        } else {
            Err(ErrorCode::Timeout)
        }
    }
}

struct MidStreamBlockingReader {
    blocked: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    use_open_stream: bool,
    ledger: Ledger,
}

impl Reader for MidStreamBlockingReader {
    fn size(&self, _: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        Ok(Some(128 << 10))
    }

    fn read_at(&self, offset: u64, _: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        if offset == 0 {
            let blob = Blob::new(&self.ledger, "stream-tests", vec![0x61; 32 << 10])?;
            return Ok(ReadResult {
                blob: Arc::new(blob),
                eof: false,
            });
        }
        self.blocked.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancel.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(3));
        }
        if cancel.is_cancelled() {
            self.cancelled.store(true, Ordering::Release);
            Err(ErrorCode::Cancelled)
        } else {
            Err(ErrorCode::Timeout)
        }
    }

    fn open_stream(
        &self,
        _: u64,
        _: Option<u64>,
        cancel: &HttpCancel,
    ) -> Result<Option<OpenedStream>, ErrorCode> {
        if !self.use_open_stream {
            return Ok(None);
        }
        Ok(Some(OpenedStream {
            first: vec![0x61; 32 << 10],
            total: Some(128 << 10),
            remaining: Some(128 << 10),
            eof: false,
            rest: Box::new(BlockingTailRest {
                blocked: Arc::clone(&self.blocked),
                cancelled: Arc::clone(&self.cancelled),
                cancel: cancel.clone(),
            }),
        }))
    }
}

#[test]
fn mid_stream_disconnect_on_open_stream_and_read_at_releases_quotas_promptly() {
    for use_open_stream in [true, false] {
        let (factory, grant, ledger) = factory();
        let bound = BoundRelay::bind().unwrap();
        let sess = session(factory, bound.authority().unwrap());
        let blocked = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        sess.commit(
            sess.prepare_reader(Arc::new(MidStreamBlockingReader {
                blocked: Arc::clone(&blocked),
                cancelled: Arc::clone(&cancelled),
                use_open_stream,
                ledger: ledger.clone(),
            }))
            .unwrap(),
        )
        .unwrap();
        let relay = bound.start(sess.clone()).unwrap();
        let url = url::Url::parse(&sess.root_url().unwrap()).unwrap();

        let mut client = TcpStream::connect(sess.authority()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            url.path(),
            sess.authority()
        )
        .unwrap();
        let mut first_bytes = [0u8; 8192];
        client.read_exact(&mut first_bytes).unwrap();
        until(|| blocked.load(Ordering::Acquire));

        let t0 = Instant::now();
        drop(client);
        until(|| {
            cancelled.load(Ordering::Acquire) && ledger.used(None, Resource::RelayWorker) == 0
        });
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "mid-stream disconnect cleanup (open_stream={use_open_stream}) took {:?}",
            t0.elapsed()
        );

        relay.stop();
        until(|| relay.is_quiescent());
        drop((relay, sess, grant));
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
    }
}

#[test]
fn cross_session_cross_window_tokens_and_cross_origin_credentials_are_isolated() {
    let (factory_a, grant, ledger) = factory();
    let factory_b = factory_a.clone();

    let bound_a = BoundRelay::bind().unwrap();
    let bound_b = BoundRelay::bind().unwrap();
    let sess_a = session(factory_a.clone(), bound_a.authority().unwrap());
    let sess_b = session(factory_b, bound_b.authority().unwrap());
    sess_a
        .commit(
            sess_a
                .prepare_reader(Arc::new(
                    MemoryReader::new(b"win-a".to_vec(), "stream-tests", &ledger).unwrap(),
                ))
                .unwrap(),
        )
        .unwrap();
    sess_b
        .commit(
            sess_b
                .prepare_reader(Arc::new(
                    MemoryReader::new(b"win-b".to_vec(), "stream-tests", &ledger).unwrap(),
                ))
                .unwrap(),
        )
        .unwrap();
    let relay_a = bound_a.start(sess_a.clone()).unwrap();
    let relay_b = bound_b.start(sess_b.clone()).unwrap();

    let root_a = url::Url::parse(&sess_a.root_url().unwrap()).unwrap();
    let path_a = root_a.path();
    let auth_b = sess_b.authority();

    // 1. Cross-window / cross-session capability token rejection over HTTP loopback.
    let wrong_session_url = format!("http://{auth_b}{path_a}");
    assert!(request(&wrong_session_url, "GET", "").starts_with(b"HTTP/1.1 403 Error\r\n"));

    let grafted_path = path_a.replacen(
        &format!("/v2/{}/", sess_a.id.0),
        &format!("/v2/{}/", sess_b.id.0),
        1,
    );
    let grafted_url = format!("http://{auth_b}{grafted_path}");
    assert!(request(&grafted_url, "GET", "").starts_with(b"HTTP/1.1 410 Error\r\n"));

    let forged_url = format!("http://{auth_b}/v2/{}/forged_token_value.bin", sess_b.id.0);
    assert!(request(&forged_url, "GET", "").starts_with(b"HTTP/1.1 410 Error\r\n"));

    relay_a.stop();
    relay_b.stop();
    until(|| relay_a.is_quiescent() && relay_b.is_quiescent());
    drop((relay_a, relay_b, sess_a, sess_b));

    // 2. Cross-origin credential isolation (Authorization, Set-Cookie CookieJar, custom headers,
    //    and cross-origin private redirect denial).
    let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr_b = listener_b.local_addr().unwrap();
    let url_b = format!("http://{addr_b}/track.vtt");
    let seen_b = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_b_bg = Arc::clone(&seen_b);
    let server_b = std::thread::spawn(move || {
        let (mut sock, _) = listener_b.accept().unwrap();
        let req = headers(&mut sock);
        seen_b_bg.lock().unwrap().push(req);
        let body = b"WEBVTT\n\n00:00.000 --> 00:01.000\nclean\n";
        write!(
            sock,
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len() - 1,
            body.len(),
            body.len()
        )
        .unwrap();
        sock.write_all(body).unwrap();
    });

    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr_a = listener_a.local_addr().unwrap();
    let url_a_seg0 = format!("http://{addr_a}/seg0.ts");
    let url_a_seg1 = format!("http://{addr_a}/seg1.ts");
    let url_a_redir = format!("http://{addr_a}/redir.ts");
    let seen_a = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_a_bg = Arc::clone(&seen_a);
    let server_a = std::thread::spawn(move || {
        for i in 0..3 {
            let (mut sock, _) = listener_a.accept().unwrap();
            let req = headers(&mut sock);
            let is_redir = req.starts_with("GET /redir.ts ");
            seen_a_bg.lock().unwrap().push(req);
            if is_redir {
                write!(
                    sock,
                    "HTTP/1.1 302 Found\r\nLocation: http://{addr_b}/track.vtt\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                let set_cookie = if i == 0 {
                    "Set-Cookie: sid=origin-a-cookie; Path=/\r\n"
                } else {
                    ""
                };
                sock.write_all(
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\n{set_cookie}Connection: close\r\n\r\ndata"
                    )
                    .as_bytes(),
                )
                .unwrap();
            }
        }
    });

    let origin_a_id = grant
        .confirm_origin(
            &url_a_seg0,
            TransportKind::Http,
            &["127.0.0.1".parse().unwrap()],
        )
        .unwrap();
    grant.confirm_insecure_auth(origin_a_id).unwrap();
    let origin_b_id = grant
        .confirm_origin(&url_b, TransportKind::Http, &["127.0.0.1".parse().unwrap()])
        .unwrap();

    // Unconfirmed plaintext Authorization on origin_b must be rejected immediately.
    let unauthorized_b = RemoteResource {
        url: url_b.clone(),
        headers: vec![("Authorization".into(), "Bearer leaked".into())],
        origin_grant: Some(origin_b_id),
    };
    assert_eq!(
        factory_a.reader(&unauthorized_b, None).err(),
        Some(ErrorCode::Denied)
    );

    let auth_headers_a = vec![
        ("Authorization".into(), "Bearer origin-a-secret".into()),
        ("X-Origin-A".into(), "private-tag".into()),
    ];
    let res_a0 = RemoteResource {
        url: url_a_seg0,
        headers: auth_headers_a.clone(),
        origin_grant: Some(origin_a_id),
    };
    let res_a1 = RemoteResource {
        url: url_a_seg1,
        headers: auth_headers_a.clone(),
        origin_grant: Some(origin_a_id),
    };
    let res_a_redir = RemoteResource {
        url: url_a_redir,
        headers: auth_headers_a,
        origin_grant: Some(origin_a_id),
    };
    let res_b = RemoteResource {
        url: url_b,
        headers: Vec::new(),
        origin_grant: Some(origin_b_id),
    };

    let doc = serde_json::to_vec(&json!({
        "resources": [
            {"id": "s0", "kind": "media", "extension": "ts", "size": "4", "source": {"transport": "remote", "request": res_a0}},
            {"id": "s1", "kind": "media", "extension": "ts", "size": "4", "source": {"transport": "remote", "request": res_a1}},
            {"id": "s_redir", "kind": "media", "extension": "ts", "size": "4", "source": {"transport": "remote", "request": res_a_redir}}
        ],
        "tracks": [{
            "id": "v",
            "kind": "video",
            "label": "Video",
            "lang": null,
            "codec": null,
            "default": true,
            "sequence": "1",
            "segments": [
                {"resource": {"resource_id": "s0", "range": null}, "duration_ms": "1000", "init": null, "key": null},
                {"resource": {"resource_id": "s1", "range": null}, "duration_ms": "1000", "init": null, "key": null},
                {"resource": {"resource_id": "s_redir", "range": null}, "duration_ms": "1000", "init": null, "key": null}
            ]
        }],
        "variants": [],
        "live": false
    }))
    .unwrap();

    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory_a.clone(), bound.authority().unwrap());
    sess.commit(
        sess.prepare_manifest(Id(0), ManifestFormat::VirtualHls, &doc)
            .unwrap(),
    )
    .unwrap();
    let sub_url = sess
        .add_attachment("sub_b", "vtt", factory_a.reader(&res_b, Some(38)).unwrap())
        .unwrap();
    let relay = bound.start(sess.clone()).unwrap();

    let m3u8 = String::from_utf8(request(&sess.root_url().unwrap(), "GET", "")).unwrap();
    let seg_urls: Vec<&str> = m3u8.lines().filter(|l| l.starts_with("http://")).collect();
    assert_eq!(seg_urls.len(), 3);

    // seg0 receives Set-Cookie; seg1 sends Cookie + Authorization to origin_a.
    assert!(request(seg_urls[0], "GET", "").ends_with(b"\r\n\r\ndata"));
    assert!(request(seg_urls[1], "GET", "").ends_with(b"\r\n\r\ndata"));
    // Cross-origin private redirect from origin_a -> origin_b fails closed (403 Denied) before connecting to origin_b.
    assert!(request(seg_urls[2], "GET", "").starts_with(b"HTTP/1.1 403 Error\r\n"));
    // Subtitle on origin_b fetches cleanly with zero credentials from origin_a.
    assert!(request(&sub_url, "GET", "").ends_with(b"clean\n"));

    server_a.join().unwrap();
    server_b.join().unwrap();

    let a_reqs = seen_a.lock().unwrap();
    assert_eq!(a_reqs.len(), 3);
    assert!(
        a_reqs[1]
            .to_ascii_lowercase()
            .contains("cookie: sid=origin-a-cookie")
    );
    assert!(
        a_reqs[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer origin-a-secret")
    );

    let b_reqs = seen_b.lock().unwrap();
    assert_eq!(
        b_reqs.len(),
        1,
        "origin_b must only receive the explicit subtitle read, never the cross-origin redirect"
    );
    let lower_b = b_reqs[0].to_ascii_lowercase();
    assert!(!lower_b.contains("authorization:"), "{lower_b}");
    assert!(!lower_b.contains("cookie:"), "{lower_b}");
    assert!(!lower_b.contains("x-origin-a:"), "{lower_b}");

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, sess, factory_a, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn unsized_and_sized_range_boundaries_and_head_and_nonzero_range_adaptive_interception() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());

    let data: Vec<u8> = (0..(256 << 10)).map(|i| (i % 251) as u8).collect();
    let unsized_reader = Arc::new(StreamingReader {
        data: Arc::new(data.clone()),
        known_size: false,
        use_open_stream: true,
        ledger: ledger.clone(),
    });
    sess.commit(sess.prepare_reader(unsized_reader).unwrap())
        .unwrap();

    // Unprobed attachments containing BOM/whitespace-padded HLS & MPD manifests.
    let bom_hls = Arc::new(
        MemoryReader::new(
            b"\xef\xbb\xbf   \n#EXTM3U\n#EXTINF:2,\nhttps://evil.example/seg.ts\n".to_vec(),
            "stream-tests",
            &ledger,
        )
        .unwrap(),
    );
    let bom_mpd = Arc::new(
        MemoryReader::new(
            b"\xef\xbb\xbf\t<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\"/>"
                .to_vec(),
            "stream-tests",
            &ledger,
        )
        .unwrap(),
    );
    let hls_attach = sess.add_attachment("smuggled_hls", "bin", bom_hls).unwrap();
    let mpd_attach = sess.add_attachment("smuggled_mpd", "bin", bom_mpd).unwrap();

    let relay = bound.start(sess.clone()).unwrap();
    let root = sess.root_url().unwrap();

    // 1. Unsized reader (`size == None`):
    //    - GET without Range and GET with `Range: bytes=0-` stream 200 OK chunked
    for extra in ["", "Range: bytes=0-\r\n"] {
        let resp = request(&root, "GET", extra);
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&resp[..split]).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
        assert!(!head.contains("Accept-Ranges:"), "{head}");
        assert_eq!(decode_chunked(&resp[split..]), data);
    }

    //    - Open-ended non-zero (`bytes=1-`), suffix (`bytes=-16`), and inverted (`bytes=9-3`) -> 416 `bytes */*`
    for bad in [
        "Range: bytes=1-\r\n",
        "Range: bytes=-16\r\n",
        "Range: bytes=9-3\r\n",
    ] {
        let resp = String::from_utf8(request(&root, "GET", bad)).unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"),
            "{resp}"
        );
        assert!(resp.contains("Content-Range: bytes */*\r\n"), "{resp}");
    }

    //    - Closed byte range (`bytes=10-73`) -> 206 Partial Content with `bytes 10-73/*`
    let closed = request(&root, "GET", "Range: bytes=10-73\r\n");
    let split = closed.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = std::str::from_utf8(&closed[..split]).unwrap();
    assert!(
        head.starts_with("HTTP/1.1 206 Partial Content\r\n"),
        "{head}"
    );
    assert!(head.contains("Content-Range: bytes 10-73/*\r\n"), "{head}");
    assert!(head.contains("Content-Length: 64\r\n"), "{head}");
    assert_eq!(&closed[split..], &data[10..74]);

    //    - HEAD on unsized reader -> 200 OK, empty body, no Transfer-Encoding: chunked
    let head_resp = String::from_utf8(request(&root, "HEAD", "")).unwrap();
    assert!(head_resp.starts_with("HTTP/1.1 200 OK\r\n"), "{head_resp}");
    assert!(
        !head_resp.contains("Transfer-Encoding: chunked"),
        "{head_resp}"
    );
    assert!(!head_resp.contains("Content-Length:"), "{head_resp}");
    assert!(head_resp.ends_with("\r\n\r\n"), "{head_resp}");

    // 2. Adaptive manifest interception on HEAD, non-zero Range (`bytes=16-31`), and full GET:
    //    never leaks raw adaptive bytes and never marks `probed`.
    for attach in [&hls_attach, &mpd_attach] {
        let head_probe = request(attach, "HEAD", "");
        assert!(head_probe.starts_with(b"HTTP/1.1 502 Error\r\n"));

        let skip_prefix_range = request(attach, "GET", "Range: bytes=16-31\r\n");
        assert!(skip_prefix_range.starts_with(b"HTTP/1.1 502 Error\r\n"));

        let full_get = request(attach, "GET", "");
        assert!(full_get.starts_with(b"HTTP/1.1 502 Error\r\n"));
        let text = String::from_utf8_lossy(&full_get);
        assert!(!text.contains("#EXTM3U") && !text.contains("<MPD"));
    }

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}
