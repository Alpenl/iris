use super::*;
use crate::{
    actor::ReadResult,
    ledger::Resource,
    stream::{
        reader::{MemoryReader, Reader},
        relay::BoundRelay,
    },
};
use iris_api::v2::ErrorCode;
use iris_base::net::http_client::HttpCancel;
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
    assert!(request(&url, "POST", "").starts_with(b"HTTP/1.1 405"));
    assert!(request(&url, "GET", "Transfer-Encoding: chunked\r\n").starts_with(b"HTTP/1.1 400"));
    assert!(request(&url, "GET", "Content-Length: 5\r\n").starts_with(b"HTTP/1.1 413"));
    assert!(
        request(&url, "GET", "Range: bytes=0-1\r\nRange: bytes=2-3\r\n")
            .starts_with(b"HTTP/1.1 416")
    );
    relay.stop();
    until(|| relay.is_quiescent());
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
    assert_eq!(ledger.used(None, Resource::StreamEntry), 0);
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

struct UnsizedChunkedReader {
    data: Vec<u8>,
    chunk: usize,
    ledger: Ledger,
}
impl Reader for UnsizedChunkedReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(None)
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
            .min(self.chunk)
            .min(self.data.len().saturating_sub(start));
        let end = start + take;
        let blob = crate::instance::Blob::new(
            &self.ledger,
            "stream-tests",
            self.data[start..end].to_vec(),
        )?;
        Ok(ReadResult {
            blob: Arc::new(blob),
            eof: end == self.data.len(),
        })
    }
}

#[test]
fn unsized_reader_multi_chunk_206_range_is_not_truncated() {
    let (factory, grant, ledger) = factory();
    let bound = BoundRelay::bind().unwrap();
    let session = session(factory, bound.authority().unwrap());
    let total = 4usize << 20;
    let payload: Vec<u8> = (0..total)
        .map(|i| ((i.wrapping_mul(29) ^ (i >> 9) ^ (i >> 17)) & 0xff) as u8)
        .collect();
    let reader = Arc::new(UnsizedChunkedReader {
        data: payload.clone(),
        chunk: 256 << 10,
        ledger: ledger.clone(),
    });
    session
        .commit(session.prepare_reader(reader).unwrap())
        .unwrap();
    let relay = bound.start(session.clone()).unwrap();
    let url = session.root_url().unwrap();

    let requested = 2usize << 20;
    let response = request(
        &url,
        "GET",
        &format!("Range: bytes=0-{}\r\n", requested - 1),
    );
    let split = response.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    let head = std::str::from_utf8(&response[..split]).unwrap();
    assert!(
        head.starts_with("HTTP/1.1 206 Partial Content\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!("Content-Length: {requested}\r\n")),
        "{head}"
    );
    assert!(
        head.contains(&format!("Content-Range: bytes 0-{}/*\r\n", requested - 1)),
        "{head}"
    );
    assert_eq!(response.len() - split, requested);
    assert_eq!(&response[split..], &payload[..requested]);

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

#[test]
fn short_initial_range_on_open_stream_rejects_adaptive_manifest_and_never_marks_probed() {
    use iris_api::v2::stream::ManifestFormat;
    use serde_json::json;

    let (factory, grant, ledger) = factory();
    let playlist =
        b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nhttps://evil.example/0.ts\n";
    let total = playlist.len();
    let range_resp = |start: usize, data: &[u8]| -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{total}\r\nETag: \"m3u\"\r\nConnection: close\r\n\r\n",
            data.len(),
            start,
            start + data.len() - 1
        )
        .into_bytes();
        out.extend_from_slice(data);
        out
    };
    let (upstream_url, upstream_rx, upstream_thread) = server(vec![
        range_resp(0, &playlist[..4]),
        range_resp(0, playlist),
        range_resp(0, playlist),
    ]);
    let remote = resource(&grant, &upstream_url);
    let doc = serde_json::to_vec(&json!({
        "resources": [{
            "id": "seg0",
            "kind": "media",
            "extension": "ts",
            "size": total.to_string(),
            "source": {"transport": "remote", "request": remote}
        }],
        "tracks": [{
            "id": "video",
            "kind": "video",
            "label": "Video",
            "lang": null,
            "codec": null,
            "default": true,
            "sequence": "1",
            "segments": [{
                "resource": {"resource_id": "seg0", "range": null},
                "duration_ms": "4000",
                "init": null,
                "key": null
            }]
        }],
        "variants": [],
        "live": false
    }))
    .unwrap();

    let bound = BoundRelay::bind().unwrap();
    let session = session(factory, bound.authority().unwrap());
    session
        .commit(
            session
                .prepare_manifest(Id(0), ManifestFormat::VirtualHls, &doc)
                .unwrap(),
        )
        .unwrap();
    let relay = bound.start(session.clone()).unwrap();

    let playlist_resp =
        String::from_utf8(request(&session.root_url().unwrap(), "GET", "")).unwrap();
    let seg_url = playlist_resp
        .lines()
        .find(|l| l.starts_with("http://"))
        .unwrap()
        .to_owned();

    // 1. Short range probe (4 bytes = "#EXT") must trigger adaptive prefix probe and fail closed (502).
    let short = request(&seg_url, "GET", "Range: bytes=0-3\r\n");
    assert!(
        short.starts_with(b"HTTP/1.1 502"),
        "{}",
        String::from_utf8_lossy(&short)
    );
    assert!(upstream_rx.recv().unwrap().contains("Range: bytes=0-3\r\n"));
    assert!(
        upstream_rx
            .recv()
            .unwrap()
            .contains(&format!("Range: bytes=0-{}\r\n", total - 1))
    );

    // 2. Subsequent full GET must still NOT be marked probed and must also reject with 502.
    let full = request(&seg_url, "GET", "");
    assert!(
        full.starts_with(b"HTTP/1.1 502"),
        "{}",
        String::from_utf8_lossy(&full)
    );
    assert!(
        upstream_rx
            .recv()
            .unwrap()
            .contains(&format!("Range: bytes=0-{}\r\n", total - 1))
    );

    relay.stop();
    until(|| relay.is_quiescent());
    upstream_thread.join().unwrap();
    drop((relay, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

#[test]
fn relay_and_prepare_reject_whitespace_padded_adaptive_manifests_at_64_and_4096_bytes() {
    use crate::stream::prepare::Preparation;
    use iris_api::{
        SourceEntryKind,
        v2::stream::{Attachment, ManifestFormat, StreamSource, StreamSpec},
    };
    use serde_json::json;

    let (factory, grant, ledger) = factory();
    let mut padded_128 = vec![b' '; 128];
    padded_128.extend_from_slice(b"#EXTM3U\n#EXTINF:4.0,\nhttps://evil.example/0.ts\n");
    let mut padded_5000 = vec![b'\n'; 5000];
    padded_5000.extend_from_slice(b"#EXTM3U\n#EXTINF:4.0,\nhttps://evil.example/0.ts\n");

    let range_resp = |total: usize, data: &[u8]| -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{total}\r\nETag: \"ws\"\r\nConnection: close\r\n\r\n",
            data.len(),
            data.len() - 1
        )
        .into_bytes();
        out.extend_from_slice(data);
        out
    };

    // 1. VirtualManifest segment with 128 leading spaces and 5000 leading newlines:
    //    a short segment is rejected from the first streamed chunk; a long whitespace
    //    prefix stays `undecided`, is probed up to 4096 bytes and rejected. Neither marks
    //    `probed`, so repeating the short-segment request must contact upstream again.
    let (seg128_url, seg128_rx, seg128_th) = server(vec![
        range_resp(padded_128.len(), &padded_128),
        range_resp(padded_128.len(), &padded_128),
    ]);
    let (seg5k_url, seg5k_rx, seg5k_th) = server(vec![
        range_resp(padded_5000.len(), &padded_5000),
        range_resp(padded_5000.len(), &padded_5000[..4096]),
    ]);
    let doc = serde_json::to_vec(&json!({
        "resources": [
            {
                "id": "s128",
                "kind": "media",
                "extension": "ts",
                "size": padded_128.len().to_string(),
                "source": {"transport": "remote", "request": resource(&grant, &seg128_url)}
            },
            {
                "id": "s5k",
                "kind": "media",
                "extension": "ts",
                "size": padded_5000.len().to_string(),
                "source": {"transport": "remote", "request": resource(&grant, &seg5k_url)}
            }
        ],
        "tracks": [{
            "id": "video",
            "kind": "video",
            "label": "Video",
            "lang": null,
            "codec": null,
            "default": true,
            "sequence": "1",
            "segments": [
                {"resource": {"resource_id": "s128", "range": null}, "duration_ms": "4000", "init": null, "key": null},
                {"resource": {"resource_id": "s5k", "range": null}, "duration_ms": "4000", "init": null, "key": null}
            ]
        }],
        "variants": [],
        "live": false
    }))
    .unwrap();

    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(
        sess.prepare_manifest(Id(0), ManifestFormat::VirtualHls, &doc)
            .unwrap(),
    )
    .unwrap();
    // Also verify read_at path via UnsizedChunkedReader attachment with 64-byte chunks of spaces.
    let chunked_ws = Arc::new(UnsizedChunkedReader {
        data: padded_5000.clone(),
        chunk: 64,
        ledger: ledger.clone(),
    });
    let attach_url = sess.add_attachment("audio_ws", "bin", chunked_ws).unwrap();
    let relay = bound.start(sess.clone()).unwrap();

    let playlist_resp = String::from_utf8(request(&sess.root_url().unwrap(), "GET", "")).unwrap();
    let seg_urls: Vec<String> = playlist_resp
        .lines()
        .filter(|l| l.starts_with("http://"))
        .map(str::to_owned)
        .collect();
    assert_eq!(seg_urls.len(), 2);

    assert!(request(&seg_urls[0], "GET", "").starts_with(b"HTTP/1.1 502"));
    assert!(request(&seg_urls[0], "GET", "").starts_with(b"HTTP/1.1 502"));
    assert!(request(&seg_urls[1], "GET", "").starts_with(b"HTTP/1.1 502"));
    assert!(request(&attach_url, "GET", "").starts_with(b"HTTP/1.1 502"));

    seg128_rx.recv().unwrap();
    seg128_rx.recv().unwrap();
    seg5k_rx.recv().unwrap();
    seg5k_rx.recv().unwrap();
    seg128_th.join().unwrap();
    seg5k_th.join().unwrap();

    relay.stop();
    until(|| relay.is_quiescent());
    drop((relay, sess));

    // 2. DirectHttp and audio attachment with >= 4096 leading whitespace bytes without EOF reject with Unsupported.
    let (direct_url, direct_rx, direct_th) =
        server(vec![range_resp(padded_5000.len(), &padded_5000[..4096])]);
    let bad_direct = StreamSpec {
        source_id: Some("ugc".into()),
        entry_id: Some("ugc:BV1:42".into()),
        title: Some("Padded".into()),
        kind: Some(SourceEntryKind::Video),
        is_live: false,
        source: StreamSource::DirectHttp {
            primary: resource(&grant, &direct_url),
            fallback_urls: Vec::new(),
            identity: None,
        },
        variants: Vec::new(),
        active_variant: None,
        audio_tracks: Vec::new(),
        subtitles: Vec::new(),
    };
    let err = Preparation {
        network: grant.clone(),
        ledger: ledger.clone(),
        generation: Generation::default(),
        guest: None,
    }
    .prepare(
        &bad_direct,
        &std::collections::BTreeMap::new(),
        &HttpCancel::default(),
    )
    .err();
    assert_eq!(err, Some(ErrorCode::Unsupported));
    direct_rx.recv().unwrap();
    direct_th.join().unwrap();

    let mp4 = b"\x00\x00\x00\x18ftypisom";
    let (v_url, v_rx, v_th) = server(vec![range_resp(mp4.len(), mp4)]);
    let (a_url, a_rx, a_th) = server(vec![range_resp(padded_5000.len(), &padded_5000[..4096])]);
    let bad_audio = StreamSpec {
        source_id: Some("ugc".into()),
        entry_id: Some("ugc:BV1:42".into()),
        title: Some("BadAudio".into()),
        kind: Some(SourceEntryKind::Video),
        is_live: false,
        source: StreamSource::DirectHttp {
            primary: resource(&grant, &v_url),
            fallback_urls: Vec::new(),
            identity: None,
        },
        variants: Vec::new(),
        active_variant: None,
        audio_tracks: vec![Attachment {
            id: "a0".into(),
            label: "PaddedAudio".into(),
            lang: None,
            resource: resource(&grant, &a_url),
            default: true,
        }],
        subtitles: Vec::new(),
    };
    let err = Preparation {
        network: grant.clone(),
        ledger: ledger.clone(),
        generation: Generation::default(),
        guest: None,
    }
    .prepare(
        &bad_audio,
        &std::collections::BTreeMap::new(),
        &HttpCancel::default(),
    )
    .err();
    assert_eq!(err, Some(ErrorCode::Unsupported));
    v_rx.recv().unwrap();
    a_rx.recv().unwrap();
    v_th.join().unwrap();
    a_th.join().unwrap();

    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}
