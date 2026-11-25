use super::*;
use crate::{
    plugin_v2::{instance::Blob, ledger::Resource, stream::reader::Reader},
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, stream::ByteIdentity};
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
        size: Id(65536),
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
    use crate::plugin_v2::stream::{prepare::probe_prefix, relay::BoundRelay};
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
    use crate::plugin_v2::stream::prepare::Preparation;
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
    use crate::plugin_v2::stream::{prepare::probe_prefix, relay::BoundRelay};
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
