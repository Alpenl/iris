use super::*;
use crate::instance::Blob;
use crate::ledger::Resource;
use crate::stream::reader::Reader;
use iris_api::v2::{ErrorCode, stream::ByteIdentity};
use iris_net::http_client::HttpCancel;
fn partial(start: usize, total: usize, data: &[u8], etag: &str) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{total}\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n",data.len(),start,start+data.len()-1).into_bytes();
    bytes.extend_from_slice(data);
    bytes
}
#[test]
fn remote_ranges_pin_representation_and_use_the_same_network_policy() {
    let (factory, grant, ledger) = factory();
    let (url, requests, thread) = server(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"a\"\r\n\r\n".to_vec(),
        partial(2, 8, b"cdef", "a"),
        partial(6, 8, b"gh", "b"),
    ]);
    let resource = resource(&grant, &url);
    let reader = factory.reader(&resource, None).unwrap();
    let cancel = HttpCancel::default();
    assert_eq!(reader.size(&cancel).unwrap(), Some(8));
    assert!(requests.recv().unwrap().starts_with("HEAD "));
    let read = reader.read_at(2, 4, &cancel).unwrap();
    assert_eq!(read.blob.bytes, b"cdef");
    assert!(!read.eof);
    drop(read);
    let sent = requests.recv().unwrap();
    assert!(sent.contains("Range: bytes=2-5\r\n"));
    assert!(sent.contains("If-Range: \"a\"\r\n"));
    assert!(matches!(
        reader.read_at(6, 2, &cancel),
        Err(ErrorCode::StaleHandle)
    ));
    requests.recv().unwrap();
    thread.join().unwrap();
    grant.revoke();
    assert!(matches!(
        reader.read_at(0, 1, &cancel),
        Err(ErrorCode::Denied)
    ));
    drop(reader);
    drop(factory);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}
#[test]
fn fallback_verifies_plaintext_before_splicing_and_does_not_trust_etags_across_cdns() {
    let (factory, grant, ledger) = factory();
    let correct: Vec<u8> = (0..65536).map(|n| (n % 251) as u8).collect();
    let wrong = vec![7; 65536];
    let (first, first_requests, first_thread) = server(vec![partial(0, 65536, &wrong, "same")]);
    let (second, second_requests, second_thread) =
        server(vec![partial(0, 65536, &correct, "different")]);
    let primary = resource(&grant, &first);
    let fallback = resource(&grant, &second);
    assert!(
        factory
            .direct(&primary, std::slice::from_ref(&fallback), None)
            .is_err()
    );
    let hash = ring::digest::digest(&ring::digest::SHA256, &correct);
    let hashes = Arc::new(Blob::new(&ledger, "stream-tests", hash.as_ref().to_vec()).unwrap());
    let identity = ByteIdentity {
        size: iris_api::v2::WireU64(65536),
        chunk_bytes: 65536,
        hashes: Id(1),
    };
    let reader = factory
        .direct(&primary, &[fallback], Some((&identity, hashes)))
        .unwrap();
    let result = reader.read_at(17, 100, &HttpCancel::default()).unwrap();
    assert_eq!(result.blob.bytes, &correct[17..117]);
    assert!(!result.eof);
    assert!(
        first_requests
            .recv()
            .unwrap()
            .contains("Range: bytes=0-65535")
    );
    assert!(
        second_requests
            .recv()
            .unwrap()
            .contains("Range: bytes=0-65535")
    );
    first_thread.join().unwrap();
    second_thread.join().unwrap();
    drop(result);
    drop(reader);
    drop(factory);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
#[test]
fn wrong_range_compressed_body_and_private_redirects_fail_closed() {
    for response in [partial(1,8,b"bc","a"),b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-1/8\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nab".to_vec(),b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/secret\r\nContent-Length: 0\r\n\r\n".to_vec()] {
        let (factory,grant,_) = factory(); let (url,rx,thread) = server(vec![response]);
        let reader = factory.reader(&resource(&grant,&url),None).unwrap();
        assert!(reader.read_at(0,2,&HttpCancel::default()).is_err()); rx.recv().unwrap();thread.join().unwrap();
    }
}

#[test]
fn continuous_stream_exceeds_chunk_limit_on_single_connection_and_tolerates_cdn_etag_on_seek() {
    use crate::stream::{prepare::probe_prefix, relay::BoundRelay};
    let (factory, grant, ledger) = factory();
    let total = (5usize << 20) + 123;
    let body: Vec<u8> = (0..total).map(|n| ((n * 31) % 251) as u8).collect();
    let probe_resp = partial(0, total, &body[..4096], "edge-a");
    let full_resp = partial(0, total, &body, "edge-b");
    // Simulate a multi-edge CDN rejecting If-Range on seek with 200 OK (same
    // length), then serving 206 Partial Content without ETag on retry.
    let seek_offset = 3usize << 20;
    let mismatch_200 = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"edge-c\"\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let mut seek_206 = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{total}\r\nConnection: close\r\n\r\n",
        total - seek_offset,
        seek_offset,
        total - 1
    )
    .into_bytes();
    seek_206.extend_from_slice(&body[seek_offset..]);

    let (url, requests, thread) = server(vec![probe_resp, full_resp, mismatch_200, seek_206]);
    let res = resource(&grant, &url);
    let reader: Arc<dyn Reader> = factory.reader(&res, None).unwrap();
    let prefix = probe_prefix(&reader, 4096, &HttpCancel::default()).unwrap();
    assert_eq!(prefix, &body[..4096]);
    assert!(requests.recv().unwrap().contains("Range: bytes=0-4095\r\n"));

    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
    let relay = bound.start(sess.clone()).unwrap();
    let root = sess.root_url().unwrap();

    // Full >5 MiB GET must use ONE upstream connection (not 3 per-2MiB reconnects).
    let got = request(&root, "GET", "");
    let split = got.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(got.starts_with(b"HTTP/1.1 200"));
    assert_eq!(&got[split..], body.as_slice());
    let stream_req = requests.recv().unwrap();
    assert!(stream_req.contains(&format!("Range: bytes=0-{}\r\n", total - 1)));
    assert!(!stream_req.contains("If-Range:"));

    // Seek across CDN edges with If-Range -> 200 fallback -> 206 retry.
    let seek = request(&root, "GET", &format!("Range: bytes={seek_offset}-\r\n"));
    let split = seek.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(seek.starts_with(b"HTTP/1.1 206"));
    assert_eq!(&seek[split..], &body[seek_offset..]);
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-b\"\r\n")
    );
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    relay.stop();
    until(|| relay.is_quiescent());
    thread.join().unwrap();
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn prepare_skips_failing_default_audio_and_promotes_working_fallback_audio() {
    use crate::stream::prepare::Preparation;
    use iris_api::{
        SourceEntryKind,
        v2::stream::{Attachment, StreamSource, StreamSpec},
    };
    let (_, grant, ledger) = factory();
    let video_bytes = vec![
        0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
    ];
    let audio_bytes = vec![
        0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2',
    ];
    let (v_url, v_rx, v_thread) = server(vec![partial(0, video_bytes.len(), &video_bytes, "v1")]);
    let (bad_a_url, bad_a_rx, bad_a_thread) = server(vec![
        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    ]);
    let (ok_a_url, ok_a_rx, ok_a_thread) =
        server(vec![partial(0, audio_bytes.len(), &audio_bytes, "a1")]);
    let spec = StreamSpec {
        source_id: Some("popular".into()),
        entry_id: Some("BV1".into()),
        title: Some("Resilient".into()),
        kind: Some(SourceEntryKind::Video),
        is_live: false,
        source: StreamSource::DirectHttp {
            primary: resource(&grant, &v_url),
            fallback_urls: Vec::new(),
            identity: None,
        },
        variants: Vec::new(),
        active_variant: None,
        audio_tracks: vec![
            Attachment {
                id: "a0".into(),
                label: "Broken".into(),
                lang: None,
                resource: resource(&grant, &bad_a_url),
                default: true,
            },
            Attachment {
                id: "a1".into(),
                label: "Working AAC".into(),
                lang: None,
                resource: resource(&grant, &ok_a_url),
                default: false,
            },
        ],
        subtitles: Vec::new(),
    };
    let playback = Preparation {
        network: grant.clone(),
        ledger: ledger.clone(),
        generation: Generation::default(),
        guest: None,
    }
    .prepare(
        &spec,
        &std::collections::BTreeMap::new(),
        &HttpCancel::default(),
    )
    .unwrap();
    assert_eq!(playback.source_id.as_deref(), Some("popular"));
    assert_eq!(playback.entry_id.as_deref(), Some("BV1"));
    assert_eq!(playback.attachments.len(), 1);
    assert_eq!(playback.attachments[0].id, "a1");
    assert!(playback.attachments[0].default);
    v_rx.recv().unwrap();
    bad_a_rx.recv().unwrap();
    ok_a_rx.recv().unwrap();
    v_thread.join().unwrap();
    bad_a_thread.join().unwrap();
    ok_a_thread.join().unwrap();
    playback.relay.stop();
    until(|| playback.relay.is_quiescent());
    drop((playback, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn continuous_stream_transparently_resumes_after_mid_stream_upstream_drop() {
    use crate::stream::{prepare::probe_prefix, relay::BoundRelay};
    let (factory, grant, ledger) = factory();
    let total = 1usize << 20;
    let body: Vec<u8> = (0..total)
        .map(|i| ((i.wrapping_mul(37) ^ (i >> 8) ^ (i >> 16)) & 0xff) as u8)
        .collect();
    let cut = 256 << 10;
    let probe_resp = partial(0, total, &body[..4096], "edge-a");
    // Upstream claims full 1 MiB 206 response, then abruptly closes after 256 KiB.
    let mut truncated = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {total}\r\nContent-Range: bytes 0-{}/{total}\r\nETag: \"edge-a\"\r\nConnection: close\r\n\r\n",
        total - 1
    )
    .into_bytes();
    truncated.extend_from_slice(&body[..cut]);
    // Resumed upstream request serves exact remaining tail starting at `cut`.
    let resumed = partial(cut, total, &body[cut..], "edge-a");

    let (url, requests, thread) = server(vec![probe_resp, truncated, resumed]);
    let res = resource(&grant, &url);
    let reader: Arc<dyn Reader> = factory.reader(&res, None).unwrap();
    let prefix = probe_prefix(&reader, 4096, &HttpCancel::default()).unwrap();
    assert_eq!(prefix, &body[..4096]);
    assert!(requests.recv().unwrap().contains("Range: bytes=0-4095\r\n"));

    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
    let relay = bound.start(sess.clone()).unwrap();
    let root = sess.root_url().unwrap();

    // Client issues ONE single GET and receives ALL 1 MiB seamlessly despite upstream drop.
    let got = request(&root, "GET", "");
    let split = got.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(got.starts_with(b"HTTP/1.1 200"));
    assert_eq!(&got[split..], body.as_slice());

    let first_req = requests.recv().unwrap();
    assert!(first_req.contains(&format!("Range: bytes=0-{}\r\n", total - 1)));
    let resume_req = requests.recv().unwrap();
    assert!(resume_req.contains(&format!("Range: bytes={cut}-{}\r\n", total - 1)));

    relay.stop();
    until(|| relay.is_quiescent());
    thread.join().unwrap();
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn read_at_retries_if_range_fallback_on_412_and_same_length_200() {
    let (factory, grant, ledger) = factory();
    let total = 16usize;
    let body = b"0123456789abcdef";
    let (url, requests, thread) = server(vec![
        partial(0, total, &body[..4], "edge-a"),
        b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        partial(4, total, &body[4..8], "edge-b"),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"edge-c\"\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
        partial(8, total, &body[8..12], "edge-c"),
        b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nETag: \"changed\"\r\nConnection: close\r\n\r\n"
            .to_vec(),
    ]);
    let res = resource(&grant, &url);
    let reader = factory.reader(&res, None).unwrap();
    let cancel = HttpCancel::default();

    assert_eq!(reader.read_at(0, 4, &cancel).unwrap().blob.bytes, b"0123");
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    // 412 fallback retries without If-Range and succeeds with edge-b ETag.
    assert_eq!(reader.read_at(4, 4, &cancel).unwrap().blob.bytes, b"4567");
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-a\"\r\n")
    );
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    // 200 OK with matching Content-Length retries without If-Range and succeeds with edge-c ETag.
    assert_eq!(reader.read_at(8, 4, &cancel).unwrap().blob.bytes, b"89ab");
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-b\"\r\n")
    );
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    // 200 OK with different Content-Length must still reject as StaleHandle.
    assert_eq!(
        reader.read_at(12, 4, &cancel).err(),
        Some(ErrorCode::StaleHandle)
    );
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-c\"\r\n")
    );

    thread.join().unwrap();
    drop((reader, factory, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn verified_reader_ignores_per_edge_etag_changes_after_probe_prefix() {
    use crate::stream::prepare::probe_prefix;
    let (factory, grant, ledger) = factory();
    let total = 131_072usize;
    let body: Vec<u8> = (0..total).map(|n| ((n * 17 + 3) % 251) as u8).collect();
    let mut hash_bytes = Vec::with_capacity(64);
    hash_bytes
        .extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &body[..65536]).as_ref());
    hash_bytes
        .extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &body[65536..]).as_ref());
    let hashes = Arc::new(Blob::new(&ledger, "stream-tests", hash_bytes).unwrap());
    let identity = ByteIdentity {
        size: iris_api::v2::WireU64(total as u64),
        chunk_bytes: 65536,
        hashes: Id(1),
    };
    let mut no_etag_chunk1 = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 65536\r\nContent-Range: bytes 65536-131071/{total}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    no_etag_chunk1.extend_from_slice(&body[65536..]);

    let (url, requests, thread) = server(vec![
        partial(0, total, &body[..65536], "edge-a"),
        partial(65536, total, &body[65536..], "edge-b"),
        no_etag_chunk1,
    ]);
    let primary = resource(&grant, &url);
    let reader = factory
        .direct(&primary, &[], Some((&identity, hashes)))
        .unwrap();
    let cancel = HttpCancel::default();

    let prefix = probe_prefix(&reader, 4096, &cancel).unwrap();
    assert_eq!(prefix, &body[..4096]);
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    // Second chunk returns a different ETag ("edge-b") — verified reader never
    // sends If-Range and never fails on edge ETag drift.
    let second = reader.read_at(65536, 128, &cancel).unwrap();
    assert_eq!(second.blob.bytes, &body[65536..65664]);
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    // Third read returns no ETag at all and still succeeds.
    let third = reader.read_at(65536 + 1024, 64, &cancel).unwrap();
    assert_eq!(third.blob.bytes, &body[65536 + 1024..65536 + 1088]);
    assert!(!requests.recv().unwrap().contains("If-Range:"));

    thread.join().unwrap();
    drop((second, third, reader, factory, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn direct_reader_skips_redundant_probe_on_initial_head_and_seek() {
    use crate::stream::relay::BoundRelay;
    let (factory, grant, ledger) = factory();
    let total = 8192usize;
    let body: Vec<u8> = (0..total).map(|n| (n % 251) as u8).collect();
    let (url, requests, thread) = server(vec![
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"direct\"\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
        partial(4096, total, &body[4096..5120], "direct"),
    ]);
    let res = resource(&grant, &url);
    let reader: Arc<dyn Reader> = factory.reader(&res, None).unwrap();
    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
    let relay = bound.start(sess.clone()).unwrap();
    let root = sess.root_url().unwrap();

    let head = request(&root, "HEAD", "");
    assert!(head.starts_with(b"HTTP/1.1 200"));
    assert!(requests.recv().unwrap().starts_with("HEAD "));

    let seek = request(&root, "GET", "Range: bytes=4096-5119\r\n");
    let split = seek.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(seek.starts_with(b"HTTP/1.1 206"));
    assert_eq!(&seek[split..], &body[4096..5120]);
    let seek_req = requests.recv().unwrap();
    assert!(
        seek_req.contains("Range: bytes=4096-5119\r\n"),
        "{seek_req}"
    );
    assert!(requests.try_recv().is_err(), "no extra 0-63 probe request");

    relay.stop();
    until(|| relay.is_quiescent());
    thread.join().unwrap();
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn open_continuous_and_resume_reject_ignored_if_range_etag_mutation() {
    let (factory, grant, ledger) = factory();
    let cancel = HttpCancel::default();

    // 1. Seek via open_stream(offset > 0) must fail closed when server ignores If-Range on 206.
    let (url1, rx1, th1) = server(vec![
        partial(0, 16, b"0123", "v1"),
        partial(4, 16, b"XXXX", "v2"),
    ]);
    let reader1 = factory.reader(&resource(&grant, &url1), None).unwrap();
    assert_eq!(reader1.read_at(0, 4, &cancel).unwrap().blob.bytes, b"0123");
    rx1.recv().unwrap();
    assert_eq!(
        reader1.open_stream(4, Some(4), &cancel).err(),
        Some(ErrorCode::StaleHandle)
    );
    assert!(rx1.recv().unwrap().contains("If-Range: \"v1\"\r\n"));
    th1.join().unwrap();
    drop(reader1);

    // 2. Mid-stream resume_upstream must fail closed when server ignores If-Range on 206.
    let total = 128usize << 10;
    let cut = 64usize << 10;
    let orig = vec![0x11u8; total];
    let mutated = vec![0x22u8; total];
    let mut truncated = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {total}\r\nContent-Range: bytes 0-{}/{total}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
        total - 1
    )
    .into_bytes();
    truncated.extend_from_slice(&orig[..cut]);
    let (url2, rx2, th2) = server(vec![truncated, partial(cut, total, &mutated[cut..], "v2")]);
    let reader2 = factory.reader(&resource(&grant, &url2), None).unwrap();
    let mut opened = reader2.open_stream(0, None, &cancel).unwrap().unwrap();
    let mut buf = vec![0u8; 32 << 10];
    let mut err = None;
    loop {
        match opened.rest.read_chunk(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert_eq!(err, Some(ErrorCode::StaleHandle));
    rx2.recv().unwrap();
    assert!(rx2.recv().unwrap().contains("If-Range: \"v1\"\r\n"));
    th2.join().unwrap();
    drop((opened, reader2, factory, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn resume_upstream_updates_etag_after_cdn_412_fallback() {
    use crate::stream::relay::BoundRelay;

    let (factory, grant, ledger) = factory();
    let total = 192usize << 10;
    let cut1 = 64usize << 10;
    let cut2 = 128usize << 10;
    let body: Vec<u8> = (0..total).map(|i| ((i * 29 + 7) % 251) as u8).collect();

    let mut seg1 = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {total}\r\nContent-Range: bytes 0-{}/{total}\r\nETag: \"edge-a\"\r\nConnection: close\r\n\r\n",
        total - 1
    )
    .into_bytes();
    seg1.extend_from_slice(&body[..cut1]);

    let precond_412 =
        b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();

    let mut seg2 = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {cut1}-{}/{total}\r\nETag: \"edge-b\"\r\nConnection: close\r\n\r\n",
        total - cut1,
        total - 1
    )
    .into_bytes();
    seg2.extend_from_slice(&body[cut1..cut2]);

    let seg3 = partial(cut2, total, &body[cut2..], "edge-b");

    let (url, requests, thread) = server(vec![seg1, precond_412, seg2, seg3]);
    let reader: Arc<dyn Reader> = factory
        .reader(&resource(&grant, &url), Some(total as u64))
        .unwrap();
    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(sess.prepare_reader(reader).unwrap()).unwrap();
    let relay = bound.start(sess.clone()).unwrap();

    let got = request(&sess.root_url().unwrap(), "GET", "");
    let split = got.windows(4).position(|p| p == b"\r\n\r\n").unwrap() + 4;
    assert!(got.starts_with(b"HTTP/1.1 200"));
    assert_eq!(&got[split..], body.as_slice());

    assert!(!requests.recv().unwrap().contains("If-Range:"));
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-a\"\r\n")
    );
    assert!(!requests.recv().unwrap().contains("If-Range:"));
    // Second resume must send updated "edge-b", not stale "edge-a".
    assert!(
        requests
            .recv()
            .unwrap()
            .contains("If-Range: \"edge-b\"\r\n")
    );

    relay.stop();
    until(|| relay.is_quiescent());
    thread.join().unwrap();
    drop((relay, sess, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}

#[test]
fn etagless_non_range_200_avoids_duplicate_gets_and_cdn_failover_clears_stale_etag() {
    let (factory, grant, ledger) = factory();
    let cancel = HttpCancel::default();

    // 1. ETag-less server ignoring Range with 200 OK:
    //    - offset == 0 succeeds in ONE GET (no duplicate retry),
    //    - offset > 0 fails with IoError (not StaleHandle) in ONE GET.
    let body = b"0123456789abcdef";
    let resp_200 = |payload: &[u8]| -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        out.extend_from_slice(payload);
        out
    };
    let (url1, rx1, th1) = server(vec![resp_200(body), resp_200(body), resp_200(body)]);
    let reader1 = factory.reader(&resource(&grant, &url1), None).unwrap();
    assert_eq!(reader1.read_at(0, 4, &cancel).unwrap().blob.bytes, b"0123");
    let req0 = rx1.recv().unwrap();
    assert!(req0.contains("Range: bytes=0-3\r\n"));
    assert!(!req0.contains("If-Range:"));
    assert_eq!(
        reader1.read_at(4, 4, &cancel).err(),
        Some(ErrorCode::IoError)
    );
    let req1 = rx1.recv().unwrap();
    assert!(req1.contains("Range: bytes=4-7\r\n"));
    assert!(!req1.contains("If-Range:"));
    assert_eq!(
        reader1.open_stream(4, Some(4), &cancel).err(),
        Some(ErrorCode::IoError)
    );
    let req2 = rx1.recv().unwrap();
    assert!(req2.contains("Range: bytes=4-7\r\n"));
    assert!(!req2.contains("If-Range:"));
    th1.join().unwrap();
    drop(reader1);

    // 2. CDN failover in read_at from ETag-bearing Edge A to ETag-less Edge B must clear
    //    the stale ETag so subsequent read_at(offset > 0) does not send If-Range: "edge-a".
    let mut no_etag_4_8 =
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/16\r\nConnection: close\r\n\r\n"
            .to_vec();
    no_etag_4_8.extend_from_slice(&body[4..8]);
    let mut no_etag_8_12 =
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 8-11/16\r\nConnection: close\r\n\r\n"
            .to_vec();
    no_etag_8_12.extend_from_slice(&body[8..12]);
    let (url2, rx2, th2) = server(vec![
        partial(0, 16, &body[..4], "edge-a"),
        b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        no_etag_4_8,
        no_etag_8_12,
    ]);
    let reader2 = factory.reader(&resource(&grant, &url2), None).unwrap();
    assert_eq!(reader2.read_at(0, 4, &cancel).unwrap().blob.bytes, b"0123");
    assert!(!rx2.recv().unwrap().contains("If-Range:"));

    assert_eq!(reader2.read_at(4, 4, &cancel).unwrap().blob.bytes, b"4567");
    assert!(rx2.recv().unwrap().contains("If-Range: \"edge-a\"\r\n"));
    assert!(!rx2.recv().unwrap().contains("If-Range:"));

    // Next read_at(8, 4) must send a single request with NO stale If-Range header.
    assert_eq!(reader2.read_at(8, 4, &cancel).unwrap().blob.bytes, b"89ab");
    let third_req = rx2.recv().unwrap();
    assert!(third_req.contains("Range: bytes=8-11\r\n"));
    assert!(
        !third_req.contains("If-Range:"),
        "stale ETag leaked after ETag-less failover: {third_req}"
    );
    th2.join().unwrap();
    drop((reader2, factory, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::Http), 0);
}
