use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

use super::super::playlist::rewrite_playlist;
use super::super::{
    CLIENT_TIMEOUT, GLOBAL_WORKERS, MAX_REQUEST, PlaybackRelay, RELAY_WORKERS, RelayWorkerBudget,
    RouteKind,
};
use iris_base::net::loopback_http::read_request;

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn decode_chunked_body(raw_body: &[u8]) -> Vec<u8> {
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
        let hex = line.trim_end_matches("\r\n");
        let size = usize::from_str_radix(hex, 16).unwrap();
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

#[test]
fn body_writes_survive_pause_longer_than_client_timeout_with_so_sndtimeo_cleared() {
    // 24 MiB exceeds both Linux and Windows Winsock auto-tuned loopback TCP buffers,
    // forcing the relay worker to hit `WouldBlock` and wait across the client pause.
    let total = 24usize << 20;
    let payload: Vec<u8> = (0..total)
        .map(|i| ((i.wrapping_mul(37) ^ (i >> 8) ^ (i >> 16)) & 0xff) as u8)
        .collect();

    for chunked_upstream in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let expected = payload.clone();
        let upstream = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = read_request(&mut sock, Duration::from_secs(5), MAX_REQUEST).unwrap();
            if chunked_upstream {
                sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .unwrap();
            } else {
                write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    expected.len()
                )
                .unwrap();
            }
            sock.write_all(&expected).unwrap();
        });

        let url = Url::parse(&format!("http://{addr}/stream.mp4")).unwrap();
        let budget = RelayWorkerBudget::default();
        let client_timeout = Duration::from_millis(150);
        let relay = PlaybackRelay::start_with_timeouts(
            url.clone(),
            BTreeMap::new(),
            &budget,
            client_timeout,
            Duration::from_millis(150),
        )
        .unwrap();
        let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
        let authority = relay.service.routes.authority;

        let mut client = TcpStream::connect(authority).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
            local.path()
        )
        .unwrap();

        // Pause 3x longer than `client_timeout` while the relay worker fills the
        // loopback send buffer. If SO_SNDTIMEO had remained set on body writes,
        // the worker would abort or corrupt chunks before the client drains.
        thread::sleep(Duration::from_millis(450));

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
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(err) => panic!("slow read failed (chunked={chunked_upstream}): {err}"),
            }
        }
        upstream.join().unwrap();

        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&raw[..split]).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        if chunked_upstream {
            assert!(head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
            let decoded = decode_chunked_body(&raw[split..]);
            assert_eq!(decoded.len(), payload.len());
            assert_eq!(decoded, payload);
        } else {
            assert!(
                head.contains(&format!("content-length: {total}\r\n")),
                "{head}"
            );
            assert_eq!(raw.len() - split, payload.len());
            assert_eq!(&raw[split..], payload.as_slice());
        }

        wait_until(Duration::from_secs(2), || {
            relay.service.workers.load(Ordering::Relaxed) == 0 && budget.lock().workers == 0
        });
    }
}

#[test]
fn pre_body_stalled_request_and_stalled_playlist_write_time_out_without_retry() {
    let playlist_hits = Arc::new(AtomicUsize::new(0));
    let ok_hits = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    // Build a valid ~3.6 MiB playlist (240,000 segments, within MAX_PLAYLIST = 4 MiB)
    // that expands past ~25 MiB when rewritten to capability URLs, guaranteeing that
    // a non-reading client saturates both Linux and Windows Winsock loopback buffers
    // during pre-body write_all.
    let mut big_m3u8 = String::with_capacity(3_800_000);
    big_m3u8.push_str("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n");
    for _ in 0..240_000 {
        big_m3u8.push_str("#EXTINF:1,\n0.ts\n");
    }
    big_m3u8.push_str("#EXT-X-ENDLIST\n");
    assert!(big_m3u8.len() < 4 * 1024 * 1024);
    let big_bytes = Arc::new(big_m3u8.into_bytes());

    let (stop_bg, pl_hits_bg, ok_hits_bg, big_bg) = (
        Arc::clone(&stop),
        Arc::clone(&playlist_hits),
        Arc::clone(&ok_hits),
        Arc::clone(&big_bytes),
    );
    let upstream = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(3));
                continue;
            };
            let big_body = Arc::clone(&big_bg);
            let pl_hits = Arc::clone(&pl_hits_bg);
            let ok_hits = Arc::clone(&ok_hits_bg);
            thread::spawn(move || {
                let Ok(raw) = read_request(&mut sock, Duration::from_secs(3), MAX_REQUEST) else {
                    return;
                };
                let req = String::from_utf8(raw).unwrap();
                if req.starts_with("GET /big.m3u8 ") {
                    pl_hits.fetch_add(1, Ordering::AcqRel);
                    let _ = write!(
                        sock,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        big_body.len()
                    );
                    let _ = sock.write_all(&big_body);
                } else {
                    ok_hits.fetch_add(1, Ordering::AcqRel);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong",
                    );
                }
            });
        }
    });

    let base = Url::parse(&format!("http://{addr}/ok.mp4")).unwrap();
    let budget = RelayWorkerBudget::default();
    let relay = PlaybackRelay::start_with_timeouts(
        base.clone(),
        BTreeMap::new(),
        &budget,
        Duration::from_millis(150),
        Duration::from_millis(150),
    )
    .unwrap();
    let authority = relay.service.routes.authority;
    let media_local = Url::parse(&relay.url(&base).unwrap()).unwrap();

    // 1. Stalled pre-request header client must receive 408 Request Timeout and release worker.
    let start = Instant::now();
    let mut stalled_req = TcpStream::connect(authority).unwrap();
    stalled_req
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stalled_req,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\n",
        media_local.path()
    )
    .unwrap();
    let mut timeout_reply = String::new();
    stalled_req.read_to_string(&mut timeout_reply).unwrap();
    assert!(
        timeout_reply.starts_with("HTTP/1.1 408 Request Timeout\r\n"),
        "got: {timeout_reply:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(ok_hits.load(Ordering::Acquire), 0);
    wait_until(Duration::from_secs(2), || {
        relay.service.workers.load(Ordering::Relaxed) == 0 && budget.lock().workers == 0
    });

    // 2. Stalled client that never reads a multi-MiB pre-body playlist write must time out
    //    via SO_SNDTIMEO without retrying upstream, releasing the worker slot cleanly.
    let playlist_url = Url::parse(&format!("http://{addr}/big.m3u8")).unwrap();
    let sealed_playlist = Url::parse(
        &relay
            .service
            .routes
            .seal(&playlist_url, RouteKind::Playlist, "m3u8")
            .unwrap(),
    )
    .unwrap();
    let mut stalled_playlist_client = TcpStream::connect(authority).unwrap();
    write!(
        stalled_playlist_client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
        sealed_playlist.path()
    )
    .unwrap();
    // Never read from `stalled_playlist_client`; wait for worker count to return to 0.
    wait_until(Duration::from_secs(8), || {
        playlist_hits.load(Ordering::Acquire) >= 1
            && relay.service.workers.load(Ordering::Relaxed) == 0
            && budget.lock().workers == 0
    });
    assert_eq!(
        playlist_hits.load(Ordering::Acquire),
        1,
        "failed pre-body playlist write must disconnect without retrying upstream"
    );
    drop(stalled_playlist_client);

    // 3. Subsequent normal request succeeds immediately.
    let mut ok_client = TcpStream::connect(authority).unwrap();
    ok_client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        ok_client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
        media_local.path()
    )
    .unwrap();
    let mut ok_reply = String::new();
    ok_client.read_to_string(&mut ok_reply).unwrap();
    assert!(ok_reply.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(ok_reply.ends_with("\r\n\r\npong"));

    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
}

#[test]
fn worker_saturation_enforces_single_wait_slot_503_reject_busy_and_global_budget() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let (gate_bg, in_flight_bg, stop_bg) =
        (Arc::clone(&gate), Arc::clone(&in_flight), Arc::clone(&stop));
    let upstream = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(2));
                continue;
            };
            let gate = Arc::clone(&gate_bg);
            let active = Arc::clone(&in_flight_bg);
            thread::spawn(move || {
                let Ok(raw) = read_request(&mut sock, Duration::from_secs(3), MAX_REQUEST) else {
                    return;
                };
                let req = String::from_utf8(raw).unwrap();
                if req.contains("Range: bytes=0-3") {
                    active.fetch_add(1, Ordering::AcqRel);
                    let (lock, cv) = &*gate;
                    let mut open = lock.lock().unwrap();
                    while !*open {
                        let (next, timeout) =
                            cv.wait_timeout(open, Duration::from_secs(5)).unwrap();
                        open = next;
                        if timeout.timed_out() {
                            break;
                        }
                    }
                    active.fetch_sub(1, Ordering::AcqRel);
                }
                let _ = sock.write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/16\r\nContent-Length: 4\r\nConnection: close\r\n\r\nokay",
                );
            });
        }
    });

    let url = Url::parse(&format!("http://{addr}/video.mp4")).unwrap();
    let shared_budget = RelayWorkerBudget::default();
    let slot_wait = Duration::from_millis(120);
    let relay1 = PlaybackRelay::start_with_timeouts(
        url.clone(),
        BTreeMap::new(),
        &shared_budget,
        Duration::from_secs(3),
        slot_wait,
    )
    .unwrap();
    let relay2 = PlaybackRelay::start_with_timeouts(
        url.clone(),
        BTreeMap::new(),
        &shared_budget,
        Duration::from_secs(3),
        slot_wait,
    )
    .unwrap();
    let relay3 = PlaybackRelay::start_with_timeouts(
        url.clone(),
        BTreeMap::new(),
        &shared_budget,
        Duration::from_secs(3),
        slot_wait,
    )
    .unwrap();
    let relay4 = PlaybackRelay::start_with_timeouts(
        url.clone(),
        BTreeMap::new(),
        &shared_budget,
        Duration::from_secs(3),
        slot_wait,
    )
    .unwrap();
    let relay5 = PlaybackRelay::start_with_timeouts(
        url.clone(),
        BTreeMap::new(),
        &shared_budget,
        Duration::from_secs(3),
        slot_wait,
    )
    .unwrap();

    let spawn_gated = |relay: &PlaybackRelay, count: usize| -> Vec<thread::JoinHandle<String>> {
        let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
        let authority = relay.service.routes.authority;
        (0..count)
            .map(|_| {
                let path = local.path().to_owned();
                thread::spawn(move || {
                    let mut c = TcpStream::connect(authority).unwrap();
                    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    write!(
                        c,
                        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=0-3\r\n\r\n"
                    )
                    .unwrap();
                    let mut out = String::new();
                    c.read_to_string(&mut out).unwrap();
                    out
                })
            })
            .collect()
    };

    // Occupy all RELAY_WORKERS (8) on relay1.
    let blocked = spawn_gated(&relay1, RELAY_WORKERS);
    wait_until(Duration::from_secs(3), || {
        in_flight.load(Ordering::Acquire) == RELAY_WORKERS
    });

    // 9th client on relay1 waits in the single queue slot and receives 503 after `slot_wait`.
    let local1 = Url::parse(&relay1.url(&url).unwrap()).unwrap();
    let auth1 = relay1.service.routes.authority;
    let t0 = Instant::now();
    let mut extra = TcpStream::connect(auth1).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        extra,
        "GET {} HTTP/1.1\r\nHost: {auth1}\r\nRange: bytes=4-7\r\n\r\n",
        local1.path()
    )
    .unwrap();
    let mut busy_reply = String::new();
    extra.read_to_string(&mut busy_reply).unwrap();
    assert!(
        busy_reply.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "expected 503 reject_busy, got {busy_reply:?}"
    );
    assert!(t0.elapsed() >= Duration::from_millis(80) && t0.elapsed() < Duration::from_secs(2));

    // Reserve the remaining 24 worker slots across relay2..relay4 -> hits GLOBAL_WORKERS (32).
    assert_eq!(GLOBAL_WORKERS, RELAY_WORKERS * 4);
    let mut held_leases = Vec::new();
    for peer_relay in [&relay2, &relay3, &relay4] {
        for _ in 0..RELAY_WORKERS {
            held_leases.push(peer_relay.service.try_reserve_worker().unwrap());
        }
    }
    assert_eq!(shared_budget.lock().workers, GLOBAL_WORKERS);

    // relay5 has 0 local workers, but shared_budget is at GLOBAL_WORKERS -> 503 after slot_wait.
    let local5 = Url::parse(&relay5.url(&url).unwrap()).unwrap();
    let auth5 = relay5.service.routes.authority;
    let mut global_extra = TcpStream::connect(auth5).unwrap();
    global_extra
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        global_extra,
        "GET {} HTTP/1.1\r\nHost: {auth5}\r\nRange: bytes=4-7\r\n\r\n",
        local5.path()
    )
    .unwrap();
    let mut global_busy = String::new();
    global_extra.read_to_string(&mut global_busy).unwrap();
    assert!(
        global_busy.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "expected global-budget 503, got {global_busy:?}"
    );

    // Release leases & gate: all workers complete and relay5 immediately serves 206.
    drop(held_leases);
    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    for h in blocked {
        assert!(h.join().unwrap().starts_with("HTTP/1.1 206"));
    }
    wait_until(Duration::from_secs(2), || shared_budget.lock().workers == 0);

    let mut recovered = TcpStream::connect(auth5).unwrap();
    recovered
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        recovered,
        "GET {} HTTP/1.1\r\nHost: {auth5}\r\nRange: bytes=4-7\r\n\r\n",
        local5.path()
    )
    .unwrap();
    let mut ok_reply = String::new();
    recovered.read_to_string(&mut ok_reply).unwrap();
    assert!(ok_reply.starts_with("HTTP/1.1 206"), "{ok_reply:?}");

    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
}

#[test]
fn mid_stream_client_disconnect_and_relay_cancel_abort_upstream_promptly() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream_aborted = Arc::new(AtomicUsize::new(0));
    let stream_started = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let (aborted_bg, started_bg, stop_bg) = (
        Arc::clone(&upstream_aborted),
        Arc::clone(&stream_started),
        Arc::clone(&stop),
    );
    let upstream = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(2));
                continue;
            };
            let aborted = Arc::clone(&aborted_bg);
            let started = Arc::clone(&started_bg);
            thread::spawn(move || {
                let Ok(_) = read_request(&mut sock, Duration::from_secs(3), MAX_REQUEST) else {
                    return;
                };
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 67108864\r\nConnection: close\r\n\r\n",
                );
                let _ = sock.write_all(&[0x42u8; 64 * 1024]);
                let _ = sock.flush();
                started.fetch_add(1, Ordering::AcqRel);
                // Hold stream open until relay cancels & closes its upstream socket.
                let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                let mut probe = [0u8; 1];
                let _ = sock.read(&mut probe);
                aborted.fetch_add(1, Ordering::AcqRel);
            });
        }
    });

    let url = Url::parse(&format!("http://{addr}/live.ts")).unwrap();
    let budget = RelayWorkerBudget::default();
    let relay = PlaybackRelay::start_with_budget(url.clone(), BTreeMap::new(), &budget).unwrap();
    let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
    let authority = relay.service.routes.authority;

    // 1. Mid-stream client disconnect after reading initial 16 KiB must abort upstream quickly.
    let mut client = TcpStream::connect(authority).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\n\r\n",
        local.path()
    )
    .unwrap();
    let mut initial = [0u8; 16 * 1024];
    client.read_exact(&mut initial).unwrap();
    wait_until(Duration::from_secs(2), || {
        stream_started.load(Ordering::Acquire) == 1
    });
    let t_disc = Instant::now();
    drop(client);
    wait_until(Duration::from_secs(2), || {
        upstream_aborted.load(Ordering::Acquire) == 1
            && relay.service.workers.load(Ordering::Relaxed) == 0
            && budget.lock().workers == 0
    });
    assert!(
        t_disc.elapsed() < Duration::from_secs(2),
        "disconnect release took {:?}",
        t_disc.elapsed()
    );

    // 2. Relay cancel / drop while a client is paused mid-stream must shut down cleanly.
    let mut paused_client = TcpStream::connect(authority).unwrap();
    write!(
        paused_client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\n\r\n",
        local.path()
    )
    .unwrap();
    wait_until(Duration::from_secs(2), || {
        stream_started.load(Ordering::Acquire) == 2
    });
    let t_cancel = Instant::now();
    relay.cancel();
    drop(relay);
    assert!(
        t_cancel.elapsed() < Duration::from_secs(2),
        "relay drop blocked caller for {:?}",
        t_cancel.elapsed()
    );
    wait_until(Duration::from_secs(2), || {
        let counts = budget.lock();
        counts.listeners == 0
            && counts.workers == 0
            && counts.reapers == 0
            && upstream_aborted.load(Ordering::Acquire) == 2
    });
    drop(paused_client);

    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
}

struct RecordingOrigin {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RecordingOrigin {
    fn start(
        handler: impl Fn(&str) -> (&'static str, String, Vec<u8>) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (req_bg, stop_bg) = (Arc::clone(&requests), Arc::clone(&stop));
        let handler = Arc::new(handler);
        let handle = thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                let Ok((mut sock, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let Ok(raw) = read_request(&mut sock, CLIENT_TIMEOUT, MAX_REQUEST) else {
                    continue;
                };
                let req = String::from_utf8(raw).unwrap();
                let path = req.split_ascii_whitespace().nth(1).unwrap().to_owned();
                req_bg.lock().unwrap().push(req);
                let (status, extra_headers, body) = handler(&path);
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(&body);
            }
        });
        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for RecordingOrigin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
    }
}

#[test]
fn cross_session_tokens_tampered_aad_and_cross_origin_credentials_are_isolated() {
    let origin_b = RecordingOrigin::start(|path| match path {
        "/cdn-video.mp4" => ("200 OK", String::new(), b"cdn-media".to_vec()),
        "/cdn-seg.ts" => ("200 OK", String::new(), b"cdn-seg".to_vec()),
        "/cdn.key" => ("200 OK", String::new(), b"0123456789abcdef".to_vec()),
        "/ext.vtt" => (
            "200 OK",
            "Content-Type: text/vtt\r\n".into(),
            b"WEBVTT\n\n00:00.000 --> 00:01.000\ncross\n".to_vec(),
        ),
        _ => ("404 Not Found", String::new(), Vec::new()),
    });
    let b_addr = origin_b.addr;

    let origin_a = RecordingOrigin::start(move |path| match path {
        "/main.mp4" => ("200 OK", String::new(), b"same-origin".to_vec()),
        "/redir.mp4" => (
            "302 Found",
            format!("Location: http://{b_addr}/cdn-video.mp4\r\n"),
            Vec::new(),
        ),
        "/same.vtt" => (
            "200 OK",
            "Content-Type: text/vtt\r\n".into(),
            b"WEBVTT\n\n00:00.000 --> 00:01.000\nsame\n".to_vec(),
        ),
        _ => ("404 Not Found", String::new(), Vec::new()),
    });

    let source_a = Url::parse(&format!("http://alice:s3cr3t@{}/main.mp4", origin_a.addr)).unwrap();
    let source_b = Url::parse(&format!("http://{}/cdn-video.mp4", origin_b.addr)).unwrap();
    let relay_a = PlaybackRelay::start(
        source_a.clone(),
        BTreeMap::from([
            ("Cookie".into(), "sid=cookie-secret-a".into()),
            ("X-Media-Key".into(), "header-secret-a".into()),
        ]),
    )
    .unwrap();
    let relay_b = PlaybackRelay::start(source_b.clone(), BTreeMap::new()).unwrap();

    let send_to = |relay: &PlaybackRelay, path: &str| -> String {
        let authority = relay.service.routes.authority;
        let mut c = TcpStream::connect(authority).unwrap();
        c.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
        write!(c, "GET {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    };

    // 1. Cross-session / cross-window & tampered ChaCha20-Poly1305 AAD token rejections.
    let sealed_a = Url::parse(&relay_a.url(&source_a).unwrap()).unwrap();
    let path_a = sealed_a.path();
    assert!(
        send_to(&relay_b, path_a).starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "token from relay_a must be rejected by relay_b"
    );
    let swapped_tag = path_a.replacen("/m/", "/k/", 1);
    assert!(send_to(&relay_a, &swapped_tag).starts_with("HTTP/1.1 403 Forbidden\r\n"));
    let swapped_ext = format!("{}.ts", path_a.strip_suffix(".mp4").unwrap());
    assert!(send_to(&relay_a, &swapped_ext).starts_with("HTTP/1.1 403 Forbidden\r\n"));
    let swapped_seq = {
        let mut parts: Vec<&str> = path_a.split('/').collect();
        parts[2] = "ff";
        parts.join("/")
    };
    assert!(send_to(&relay_a, &swapped_seq).starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(origin_a.requests.lock().unwrap().is_empty());
    assert!(origin_b.requests.lock().unwrap().is_empty());

    // 2a. Private/loopback source pinning: same-origin media & subtitle receive credentials,
    //     whereas cross-origin private redirects, cross-origin HLS segments/keys, and cross-origin
    //     subtitles fail closed (502) BEFORE connecting to `origin_b` (zero credential leak).
    let ok_same = send_to(&relay_a, path_a);
    assert!(ok_same.ends_with("\r\n\r\nsame-origin"));

    let same_sub_url = Url::parse(&format!("http://{}/same.vtt", origin_a.addr)).unwrap();
    let same_sub_local = Url::parse(&relay_a.subtitle_url(&same_sub_url).unwrap()).unwrap();
    assert!(send_to(&relay_a, same_sub_local.path()).contains("same\n"));

    let redir_url = Url::parse(&format!("http://{}/redir.mp4", origin_a.addr)).unwrap();
    let redir_local = Url::parse(&relay_a.url(&redir_url).unwrap()).unwrap();
    assert!(send_to(&relay_a, redir_local.path()).starts_with("HTTP/1.1 502 Bad Gateway\r\n"));

    let cross_sub_url = Url::parse(&format!("http://{}/ext.vtt", origin_b.addr)).unwrap();
    let cross_sub_local = Url::parse(&relay_a.subtitle_url(&cross_sub_url).unwrap()).unwrap();
    assert!(send_to(&relay_a, cross_sub_local.path()).starts_with("HTTP/1.1 502 Bad Gateway\r\n"));

    let playlist = format!(
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"http://{b_addr}/cdn.key\"\n#EXTINF:2,\nhttp://{b_addr}/cdn-seg.ts\n#EXT-X-ENDLIST\n"
    );
    let rewritten = rewrite_playlist(&playlist, &source_a, &relay_a.service.routes).unwrap();
    for line in rewritten.lines() {
        let raw_url = if let Some((_, tail)) = line.split_once("URI=\"") {
            tail.split_once('"').unwrap().0
        } else if line.starts_with("http://") {
            line
        } else {
            continue;
        };
        let local = Url::parse(raw_url).unwrap();
        assert!(send_to(&relay_a, local.path()).starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    }

    {
        let a_reqs = origin_a.requests.lock().unwrap();
        assert_eq!(a_reqs.len(), 3);
        for req in a_reqs.iter() {
            let lower = req.to_ascii_lowercase();
            assert!(lower.contains("authorization: basic "), "{req}");
            assert!(lower.contains("cookie: sid=cookie-secret-a"), "{req}");
            assert!(lower.contains("x-media-key: header-secret-a"), "{req}");
        }
        assert!(
            origin_b.requests.lock().unwrap().is_empty(),
            "private cross-origin pivot must be blocked before connecting to origin_b"
        );
    }

    // 2b. Reachable cross-origin destination (modeled via `ConnectionPolicy` allowing both
    //     origins, matching public CDN redirects/segments/subtitles): requests succeed on
    //     `origin_b` with ALL `origin_a` credentials (`Authorization`, `Cookie`, `X-Media-Key`,
    //     and URL userinfo) stripped.
    struct AllowLoopbackOrigins(iris_base::net::http_client::HttpCancel);
    impl iris_base::net::http_client::ConnectionPolicy for AllowLoopbackOrigins {
        fn check_url(&self, _: &Url) -> anyhow::Result<()> {
            Ok(())
        }
        fn check_address(&self, _: &Url, _: SocketAddr) -> anyhow::Result<()> {
            Ok(())
        }
        fn cancel(&self) -> &iris_base::net::http_client::HttpCancel {
            &self.0
        }
        fn reserve_dns(&self) -> anyhow::Result<Box<dyn std::any::Any + Send + Sync>> {
            Ok(Box::new(()))
        }
    }

    let cancel = iris_base::net::http_client::HttpCancel::default();
    let scope = iris_base::net::http_client::NetworkScope::authorized(
        source_a.clone(),
        Arc::new(AllowLoopbackOrigins(cancel.clone())),
    )
    .unwrap();
    let secret_headers = BTreeMap::from([
        ("Cookie".into(), "sid=cookie-secret-a".into()),
        ("X-Media-Key".into(), "header-secret-a".into()),
    ]);

    let mut redir_stream = iris_base::net::http_client::open_stream(
        iris_base::net::http_client::Request {
            method: "GET",
            url: redir_url,
            headers: &secret_headers,
            body: None,
            range: None,
        },
        &scope,
        &cancel,
        None,
    )
    .unwrap();
    let mut redir_body = Vec::new();
    redir_stream.body().read_to_end(&mut redir_body).unwrap();
    assert_eq!(redir_body, b"cdn-media");

    let mut seg_stream = iris_base::net::http_client::open_stream(
        iris_base::net::http_client::Request {
            method: "GET",
            url: Url::parse(&format!("http://{b_addr}/cdn-seg.ts")).unwrap(),
            headers: &secret_headers,
            body: None,
            range: None,
        },
        &scope,
        &cancel,
        None,
    )
    .unwrap();
    let mut seg_body = Vec::new();
    seg_stream.body().read_to_end(&mut seg_body).unwrap();
    assert_eq!(seg_body, b"cdn-seg");

    let sub_bytes = iris_base::net::http_client::fetch_subtitle_bytes(
        cross_sub_url.clone(),
        &BTreeMap::new(),
        &cross_sub_url,
        &scope,
        &cancel,
    )
    .unwrap();
    assert!(sub_bytes.ends_with(b"cross\n"));

    let b_reqs = origin_b.requests.lock().unwrap();
    assert_eq!(
        b_reqs.len(),
        3,
        "expected redirect + segment + subtitle on origin_b"
    );
    for req in b_reqs.iter() {
        let lower = req.to_ascii_lowercase();
        assert!(
            !lower.contains("authorization:"),
            "leaked Authorization to origin_b: {req}"
        );
        assert!(
            !lower.contains("cookie:"),
            "leaked Cookie to origin_b: {req}"
        );
        assert!(
            !lower.contains("x-media-key:"),
            "leaked custom header to origin_b: {req}"
        );
        assert!(
            !req.contains("s3cr3t"),
            "leaked userinfo secret to origin_b: {req}"
        );
    }
}

#[test]
fn range_out_of_bounds_unknown_length_head_and_bom_adaptive_interception() {
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    let addr = origin.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);

    let upstream = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = origin.accept() else {
                thread::sleep(Duration::from_millis(2));
                continue;
            };
            let Ok(raw) = read_request(&mut sock, CLIENT_TIMEOUT, MAX_REQUEST) else {
                continue;
            };
            let req = String::from_utf8(raw).unwrap();
            let first = req.lines().next().unwrap_or("");
            if first.starts_with("GET /unsized ") {
                sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstreamed-tail")
                    .unwrap();
            } else if first.starts_with("HEAD /unsized ") {
                sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .unwrap();
            } else if first.starts_with("GET /oob ") {
                sock.write_all(
                    b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */64\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            } else if first.starts_with("GET /bom-hls.ts ") {
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 31\r\nConnection: close\r\n\r\n\xef\xbb\xbf  \n#EXTM3U\n#EXTINF:1,\n0.ts\n",
                )
                .unwrap();
            } else if first.starts_with("GET /bom-mpd.mp4 ") {
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 43\r\nConnection: close\r\n\r\n\xef\xbb\xbf\t\r\n<?xml version=\"1.0\"?><MPD></MPD>",
                )
                .unwrap();
            }
        }
    });

    let base = Url::parse(&format!("http://{addr}/unsized")).unwrap();
    let relay = PlaybackRelay::start(base.clone(), BTreeMap::new()).unwrap();
    let authority = relay.service.routes.authority;

    let raw_exchange = |method: &str, path: &str, extra: &str| -> Vec<u8> {
        let mut c = TcpStream::connect(authority).unwrap();
        c.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
        write!(
            c,
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n"
        )
        .unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).unwrap();
        out
    };

    let unsized_local = Url::parse(&relay.url(&base).unwrap()).unwrap();
    // 1. Invalid client Ranges (`bytes=-0`, inverted, multi-range, non-bytes) fail closed with 416 + `bytes */*`.
    for bad_range in ["bytes=-0", "bytes=30-10", "bytes=0-1,2-3", "items=0-10"] {
        let resp = String::from_utf8(raw_exchange(
            "GET",
            unsized_local.path(),
            &format!("Range: {bad_range}\r\n"),
        ))
        .unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"),
            "{resp}"
        );
        assert!(resp.contains("Content-Range: bytes */*\r\n"), "{resp}");
    }

    // 2. Upstream 416 out-of-bounds is forwarded with 416 status and empty body.
    let oob_url = Url::parse(&format!("http://{addr}/oob")).unwrap();
    let oob_local = Url::parse(&relay.url(&oob_url).unwrap()).unwrap();
    let oob_resp = String::from_utf8(raw_exchange(
        "GET",
        oob_local.path(),
        "Range: bytes=500-600\r\n",
    ))
    .unwrap();
    assert!(oob_resp.starts_with("HTTP/1.1 416 "), "{oob_resp}");
    assert!(oob_resp.ends_with("\r\n\r\n"), "{oob_resp}");

    // 3. Unknown upstream total length (`None`): GET uses chunked framing, HEAD has empty body & no chunked header.
    let get_unsized = raw_exchange("GET", unsized_local.path(), "");
    let split = get_unsized
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap()
        + 4;
    let get_head = std::str::from_utf8(&get_unsized[..split]).unwrap();
    assert!(get_head.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(get_head.contains("Transfer-Encoding: chunked\r\n"));
    assert_eq!(decode_chunked_body(&get_unsized[split..]), b"streamed-tail");

    let head_unsized = String::from_utf8(raw_exchange("HEAD", unsized_local.path(), "")).unwrap();
    assert!(head_unsized.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(!head_unsized.contains("Transfer-Encoding: chunked"));
    assert!(head_unsized.ends_with("\r\n\r\n"));

    // 4. BOM + whitespace-prefixed HLS & MPD on Media route fail closed (502) on both full GET and Range: bytes=0-63.
    for blocked in ["/bom-hls.ts", "/bom-mpd.mp4"] {
        let u = Url::parse(&format!("http://{addr}{blocked}")).unwrap();
        let local = Url::parse(&relay.url(&u).unwrap()).unwrap();
        for extra in ["", "Range: bytes=0-63\r\n"] {
            let reply = String::from_utf8(raw_exchange("GET", local.path(), extra)).unwrap();
            assert!(
                reply.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
                "expected 502 for {blocked} ({extra:?}), got {reply:?}"
            );
            assert!(!reply.contains("#EXTM3U") && !reply.contains("<MPD"));
        }
    }

    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
}
