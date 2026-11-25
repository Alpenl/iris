use super::*;
use crate::{
    plugin_v2::{
        actor::ReadResult,
        ledger::Resource,
        stream::{
            reader::{MemoryReader, Reader},
            relay::BoundRelay,
        },
    },
    vfs::HttpCancel,
};
use iris_api::v2::ErrorCode;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn actual_relay_handles_head_ranges_empty_files_and_browser_denial() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let session = session(factory, bound.authority().unwrap());
    let bytes: Vec<u8> = (0..(2 << 20) + 37).map(|n| (n % 251) as u8).collect();
    let reader = Arc::new(MemoryReader::new(bytes.clone(), "stream-tests", &ledger).unwrap());
    session
        .commit(session.prepare_reader(reader).unwrap())
        .unwrap();
    let relay = bound.start(session.clone()).unwrap();
    let url = session.root_url().unwrap();
    let all = request(&url, "GET", "");
    let split = all.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(&all[split..], bytes);
    assert!(all.starts_with(b"HTTP/1.1 200"));
    let head = request(&url, "HEAD", "");
    assert!(head.ends_with(b"\r\n\r\n"));
    assert!(
        String::from_utf8(head)
            .unwrap()
            .contains(&format!("Content-Length: {}", bytes.len()))
    );
    for _ in 0..2 {
        let range = request(&url, "GET", "Range: bytes=17-27\r\n");
        assert!(range.starts_with(b"HTTP/1.1 206"));
        assert!(range.ends_with(&bytes[17..28]));
    }
    let suffix = request(&url, "GET", "Range: bytes=-10\r\n");
    assert!(suffix.ends_with(&bytes[bytes.len() - 10..]));
    let invalid = request(&url, "GET", &format!("Range: bytes={}-\r\n", bytes.len()));
    assert!(invalid.starts_with(b"HTTP/1.1 416"));
    assert!(
        String::from_utf8(invalid)
            .unwrap()
            .contains(&format!("Content-Range: bytes */{}", bytes.len()))
    );
    for header in [
        "Origin: https://evil.example\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Host: evil.example\r\n",
    ] {
        assert!(request(&url, "GET", header).starts_with(b"HTTP/1.1 403"));
    }
    relay.stop();
    until(|| relay.is_quiescent());
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
    assert_eq!(ledger.used(None, Resource::StreamResource), 0);
    drop(relay);
    drop(session);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
struct Slow {
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}
impl Reader for Slow {
    fn size(&self, _: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        Ok(Some(4))
    }
    fn read_at(&self, _: u64, _: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        self.started.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(3);
        while !cancel.is_cancelled() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(3));
        }
        if !cancel.is_cancelled() {
            return Err(ErrorCode::Timeout);
        }
        self.cancelled.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(250));
        Err(ErrorCode::Cancelled)
    }
}
#[test]
fn client_disconnect_and_session_stop_cancel_reads_without_joining_ui() {
    for disconnect in [true, false] {
        let (factory, grant, ledger) = factory();
        let bound = BoundRelay::bind().unwrap();
        let session = session(factory, bound.authority().unwrap());
        let started = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        session
            .commit(
                session
                    .prepare_reader(Arc::new(Slow {
                        started: started.clone(),
                        cancelled: cancelled.clone(),
                    }))
                    .unwrap(),
            )
            .unwrap();
        let relay = bound.start(session.clone()).unwrap();
        let url = url::Url::parse(&session.root_url().unwrap()).unwrap();
        let mut client = TcpStream::connect(session.authority()).unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            url.path(),
            session.authority()
        )
        .unwrap();
        until(|| started.load(Ordering::Acquire));
        let start = Instant::now();
        if disconnect {
            client.shutdown(std::net::Shutdown::Both).unwrap();
        } else {
            relay.stop();
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        until(|| cancelled.load(Ordering::Acquire));
        assert_eq!(ledger.used(None, Resource::RelayWorker), 1);
        assert!(!relay.is_quiescent());
        relay.stop();
        until(|| relay.is_quiescent());
        drop(client);
        drop(relay);
        drop(session);
        drop(grant);
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
    }
}

#[test]
fn slow_client_backpressure_never_duplicates_or_corrupts_stream_bytes() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let session = session(factory, bound.authority().unwrap());
    // Unique per-offset pattern across 4 MiB so any duplicated, dropped, or
    // shifted 64 KiB block is caught immediately.
    let total = 4usize << 20;
    let expected: Vec<u8> = (0..total)
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 8) ^ (i >> 16)) & 0xff) as u8)
        .collect();
    let reader = Arc::new(MemoryReader::new(expected.clone(), "stream-tests", &ledger).unwrap());
    session
        .commit(session.prepare_reader(reader).unwrap())
        .unwrap();
    let relay = bound.start(session.clone()).unwrap();
    let url = url::Url::parse(&session.root_url().unwrap()).unwrap();
    let mut client = TcpStream::connect(session.authority()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.path(),
        session.authority()
    )
    .unwrap();

    // Pause before draining and throttle reads to guarantee loopback TCP send
    // buffer saturation and repeated WouldBlock backpressure cycles.
    std::thread::sleep(Duration::from_millis(250));
    let mut raw = Vec::with_capacity(total + 512);
    let mut chunk = [0u8; 8192];
    let mut reads = 0usize;
    loop {
        match client.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                reads += 1;
                if reads.is_multiple_of(32) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Err(e) => panic!("slow relay read failed: {e}"),
        }
    }
    let split = raw.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(raw.starts_with(b"HTTP/1.1 200"));
    assert_eq!(raw.len() - split, expected.len());
    assert_eq!(&raw[split..], expected.as_slice());

    relay.stop();
    until(|| relay.is_quiescent());
    drop((client, relay, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}
