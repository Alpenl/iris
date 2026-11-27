//! A09 long live-window rolling, continuous stream-context evolution,
//! old-capability revocation, control-error isolation, and Ledger zeroing acceptance.

use super::*;
use crate::{
    actor::{Actor, Executor, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    instance::Blob,
    ledger::Resource,
    pool::IoPool,
    stream::{
        guest::StreamStore,
        reader::MemoryReader,
        relay::BoundRelay,
        remote_manifest::{convert, live},
    },
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, Role, WireU64,
    stream::{ManifestFormat, StreamContext},
    task::Task,
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::CancelToken;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const ALL_RESOURCES: [Resource; 21] = [
    Resource::WasmBytes,
    Resource::HostBytes,
    Resource::ControlStore,
    Resource::StreamStore,
    Resource::Task,
    Resource::Handle,
    Resource::Http,
    Resource::ControlHttp,
    Resource::WebSocket,
    Resource::Tcp,
    Resource::StreamRead,
    Resource::FileHash,
    Resource::TempBytes,
    Resource::Dns,
    Resource::Auth,
    Resource::PlaybackSession,
    Resource::StreamEntry,
    Resource::RelayWorker,
    Resource::OverlayCues,
    Resource::OverlayBytes,
    Resource::EnhancementTrack,
];

fn assert_ledger_zero(ledger: &Ledger) {
    until(|| ledger.used(None, Resource::HostBytes) == 0);
    for res in ALL_RESOURCES {
        assert_eq!(
            ledger.used(None, res),
            0,
            "Ledger leaked non-zero quota for {res:?}"
        );
    }
}

fn split_http(raw: &[u8]) -> (&str, &[u8]) {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (std::str::from_utf8(&raw[..split]).unwrap(), &raw[split..])
}

#[test]
fn a09_remote_live_hls_multi_round_rolling_key_rotation_transient_error_and_revocation() {
    #[derive(Clone, Copy)]
    struct LiveState {
        round: usize,
        fail_playlist: bool,
    }

    let state = Arc::new(Mutex::new(LiveState {
        round: 0,
        fail_playlist: false,
    }));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let playlist_fetches = Arc::new(AtomicUsize::new(0));

    let (state_bg, stop_bg, fetches_bg) = (
        Arc::clone(&state),
        Arc::clone(&stop),
        Arc::clone(&playlist_fetches),
    );
    let upstream = std::thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            sock.set_nonblocking(false).unwrap();
            let req = headers(&mut sock);
            let line = req.lines().next().unwrap_or("");
            let method = line.split_ascii_whitespace().next().unwrap_or("");
            let target = line.split_ascii_whitespace().nth(1).unwrap_or("");
            let has_range = req
                .lines()
                .any(|l| l.to_ascii_lowercase().starts_with("range:"));
            let write_media = |sock: &mut TcpStream, body: &[u8]| {
                if method == "HEAD" {
                    write!(
                        sock,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                } else if has_range {
                    write!(
                        sock,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len(),
                        body.len()
                    )
                    .unwrap();
                    sock.write_all(body).unwrap();
                } else {
                    write!(
                        sock,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    sock.write_all(body).unwrap();
                }
            };
            if target == "/live.m3u8" {
                fetches_bg.fetch_add(1, Ordering::AcqRel);
                let st = *state_bg.lock().unwrap();
                if st.fail_playlist {
                    sock.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    continue;
                }
                let seq = 100 + st.round;
                // Sliding window of 3 segments; round advances `seq`, rotates AES-128 key every
                // 2 rounds, and rotates signed query token on the newest segment.
                let key_ver = (st.round / 2) + 1;
                let mut m3u8 = format!(
                    "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{seq}\n#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/k{key_ver}.key\",IV=0x{:032x}\n",
                    seq
                );
                for idx in seq..(seq + 3) {
                    let sig = if idx == seq + 2 { st.round } else { 0 };
                    m3u8.push_str(&format!("#EXTINF:0.4,\n/seg/{idx}.ts?sig={sig}\n"));
                }
                write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    m3u8.len(),
                    m3u8
                )
                .unwrap();
            } else if let Some(rest) = target.strip_prefix("/keys/k") {
                let ver: u8 = rest.strip_suffix(".key").unwrap().parse().unwrap();
                let key_bytes = [ver.wrapping_mul(0x11); 16];
                write_media(&mut sock, &key_bytes);
            } else if let Some(rest) = target.strip_prefix("/seg/") {
                let (num_part, query) = rest.split_once(".ts?sig=").unwrap();
                let seg_no: usize = num_part.parse().unwrap();
                let sig: usize = query.parse().unwrap();
                let body = format!("segment-{seg_no}-sig-{sig}").into_bytes();
                write_media(&mut sock, &body);
            } else {
                sock.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            }
        }
    });

    let (factory, grant, ledger) = factory();
    let live_url = format!("http://{addr}/live.m3u8");
    let source = resource(&grant, &live_url);
    let mut initial = convert(&factory, &source, &HttpCancel::default()).unwrap();
    assert!(initial.live);
    initial.refresh_after = Duration::ZERO;

    let bound = BoundRelay::bind().unwrap();
    let sess = session(factory, bound.authority().unwrap());
    sess.commit(
        sess.prepare_manifest(Id(0), initial.format, &initial.bytes)
            .unwrap(),
    )
    .unwrap();
    live::attach(&sess, source, &initial).unwrap();

    // Mount external audio & subtitle attachments that must survive all live refreshes.
    let ext_audio = sess
        .add_attachment(
            "Commentary_AAC",
            "bin",
            Arc::new(
                MemoryReader::new(
                    b"\x00\x00\x00\x18ftypM4A ".to_vec(),
                    "stream-tests",
                    &ledger,
                )
                .unwrap(),
            ),
        )
        .unwrap();
    let ext_sub = sess
        .add_attachment(
            "Live_Captions",
            "vtt",
            Arc::new(
                MemoryReader::new(
                    b"WEBVTT\n\n00:00.000 --> 00:05.000\nLive caption\n".to_vec(),
                    "stream-tests",
                    &ledger,
                )
                .unwrap(),
            ),
        )
        .unwrap();

    let relay = bound.start(sess.clone()).unwrap();
    let root_url = sess.root_url().unwrap();

    let parse_window = |text: &str| -> (String, Vec<String>) {
        let key_url = text
            .lines()
            .find_map(|l| l.split("URI=\"").nth(1)?.split('"').next())
            .unwrap()
            .to_owned();
        let segs = text
            .lines()
            .filter(|l| l.starts_with("http://"))
            .map(str::to_owned)
            .collect();
        (key_url, segs)
    };

    let mut evicted_urls: Vec<String> = Vec::new();
    let mut prev_window: Option<(usize, String, Vec<String>)> = None;

    // Slide across 6 rounds (100..=105), injecting a transient 503 failure before round 4.
    for round in 0..6 {
        if round == 4 {
            std::thread::sleep(Duration::from_millis(270));
            state.lock().unwrap().fail_playlist = true;
            let failed = request(&root_url, "GET", "");
            assert!(
                failed.starts_with(b"HTTP/1.1 502 Error\r\n"),
                "refresh failure must surface error instead of silently ignoring: {:?}",
                String::from_utf8_lossy(&failed)
            );
            // Attachments remain readable even during transient manifest refresh failure.
            assert!(request(&ext_sub, "GET", "").ends_with(b"Live caption\n"));
            // Advance past the 1s error backoff in `live::Live` so recovery is exercised end-to-end.
            state.lock().unwrap().fail_playlist = false;
            std::thread::sleep(Duration::from_millis(1050));
        } else if round > 0 {
            if round == 1 {
                // Within the 250ms `refresh_after` window, `live::Live` serves the cached snapshot
                // without hitting upstream again.
                let before = playlist_fetches.load(Ordering::Acquire);
                let cached = request(&root_url, "GET", "");
                assert!(cached.starts_with(b"HTTP/1.1 200 OK\r\n"));
                assert_eq!(playlist_fetches.load(Ordering::Acquire), before);
            }
            std::thread::sleep(Duration::from_millis(270));
        }

        state.lock().unwrap().round = round;
        let raw_pl = request(&root_url, "GET", "");
        let (pl_head, pl_body) = split_http(&raw_pl);
        assert!(pl_head.starts_with("HTTP/1.1 200 OK\r\n"), "{pl_head}");
        let pl_text = std::str::from_utf8(pl_body).unwrap();
        let seq = 100 + round;
        assert!(
            pl_text.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{seq}\n")),
            "round {round} playlist: {pl_text}"
        );
        // Never leak upstream host/path into rendered VirtualHls.
        assert!(!pl_text.contains(&addr.to_string()));
        assert!(!pl_text.contains("?sig="));

        let (key_url, seg_urls) = parse_window(pl_text);
        assert_eq!(seg_urls.len(), 3);

        // Key resource returns exact rotated 16-byte AES key.
        let key_ver = ((round / 2) + 1) as u8;
        let raw_key = request(&key_url, "GET", "");
        let (_, key_body) = split_http(&raw_key);
        assert_eq!(key_body, &[key_ver.wrapping_mul(0x11); 16]);

        // All 3 segments in the active window fetch cleanly with expected sequence & signature.
        for (offset_idx, seg_url) in seg_urls.iter().enumerate() {
            let seg_no = seq + offset_idx;
            let expected_sig = if offset_idx == 2 { round } else { 0 };
            let raw_seg = request(seg_url, "GET", "");
            let (seg_head, seg_body) = split_http(&raw_seg);
            assert!(seg_head.starts_with("HTTP/1.1 200 OK\r\n"), "{seg_head}");
            assert_eq!(
                seg_body,
                format!("segment-{seg_no}-sig-{expected_sig}").as_bytes()
            );
        }

        if let Some((prev_round, prev_key, prev_segs)) =
            prev_window.replace((round, key_url.clone(), seg_urls.clone()))
        {
            // `prev_segs[0]` (seq - 1) rolled out of the sliding window -> must be revoked!
            evicted_urls.push(prev_segs[0].clone());
            // `prev_segs[2]` had `?sig=<prev_round>` and was rebound to `?sig=0` at `seg_urls[1]`
            // when `prev_round > 0`: rebinding a logical ID to a new URL must mint a fresh token
            // and revoke the old token!
            if prev_round > 0 {
                assert_ne!(prev_segs[2], seg_urls[1]);
                evicted_urls.push(prev_segs[2].clone());
            } else {
                // At round 0 -> 1, segment 101 had sig=0 in both rounds so its token is preserved!
                assert_eq!(prev_segs[1], seg_urls[0]);
            }
            if round / 2 != prev_round / 2 {
                assert_ne!(prev_key, key_url);
                evicted_urls.push(prev_key);
            } else {
                assert_eq!(prev_key, key_url);
            }
        }

        // Every rolled-out or rebound capability URL must reject with 410 StaleHandle.
        for old_url in &evicted_urls {
            let denied = request(old_url, "GET", "");
            assert!(
                denied.starts_with(b"HTTP/1.1 410 Error\r\n"),
                "evicted capability {old_url} remained accessible in round {round}: {:?}",
                String::from_utf8_lossy(&denied)
            );
        }

        // External audio & subtitle attachments remain intact across every refresh.
        assert_eq!(
            split_http(&request(&ext_audio, "GET", "")).1,
            b"\x00\x00\x00\x18ftypM4A "
        );
        assert_eq!(
            split_http(&request(&ext_sub, "GET", "")).1,
            b"WEBVTT\n\n00:00.000 --> 00:05.000\nLive caption\n"
        );
    }

    assert!(!evicted_urls.is_empty());
    assert!(playlist_fetches.load(Ordering::Acquire) >= 7);

    // Explicit origin revocation immediately blocks new reads on active segments.
    let (_, active_segs) =
        parse_window(std::str::from_utf8(split_http(&request(&root_url, "GET", "")).1).unwrap());
    grant.revoke();
    let after_revoke = request(&active_segs[0], "GET", "");
    assert!(
        after_revoke.starts_with(b"HTTP/1.1 403 Error\r\n"),
        "expected 403 Denied after grant revocation, got {:?}",
        String::from_utf8_lossy(&after_revoke)
    );

    relay.stop();
    until(|| relay.is_quiescent());
    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
    drop((initial, relay, sess, grant));
    assert_ledger_zero(&ledger);
}

const WAT_FIND_FN: &str = r#"(func $find (param $hp i32) (param $hl i32) (param $np i32) (param $nl i32) (result i32)
    (local $i i32)
    (local $j i32)
    (local $limit i32)
    (if (i32.gt_u (local.get $nl) (local.get $hl))
        (then (return (i32.const -1))))
    (local.set $limit (i32.sub (local.get $hl) (local.get $nl)))
    (block $done
        (loop $outer
            (br_if $done (i32.gt_u (local.get $i) (local.get $limit)))
            (local.set $j (i32.const 0))
            (block $mismatch
                (loop $inner
                    (if (i32.eq (local.get $j) (local.get $nl))
                        (then (return (i32.add (i32.add (local.get $hp) (local.get $i)) (local.get $nl)))))
                    (br_if $mismatch
                        (i32.ne
                            (i32.load8_u (i32.add (i32.add (local.get $hp) (local.get $i)) (local.get $j)))
                            (i32.load8_u (i32.add (local.get $np) (local.get $j)))))
                    (local.set $j (i32.add (local.get $j) (i32.const 1)))
                    (br $inner)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $outer)))
    (i32.const -1))"#;

fn build_a09_stream_guest_wasm() -> Vec<u8> {
    let empty = r#"{"ok":[]}"#;
    let ctx_needle = r#""context_blob":""#;
    let req_needle = r#""request_id":""#;
    let seg_needle = r#""resource_id":"seg_r"#;
    let hang_needle = r#""resource_id":"hang""#;
    let off_needle = r#""offset":""#;
    let len_needle = r#""len":"#;
    let esc = |s: &str| -> String { s.as_bytes().iter().map(|b| format!("\\{b:02x}")).collect() };
    let empty_packed = (1024_u64 << 32) | empty.len() as u64;

    wat::parse_str(format!(
        r#"(module
        (import "iris_v2" "host_blob_new" (func $blob_new (param i32 i32) (result i64)))
        (import "iris_v2" "host_blob_len" (func $blob_len (param i64) (result i64)))
        (import "iris_v2" "host_blob_read" (func $blob_read (param i64 i64 i32 i32) (result i64)))
        (import "iris_v2" "host_close" (func $host_close (param i64) (result i32)))
        (import "iris_v2" "host_stream_reply" (func $stream_reply (param i64 i64 i32) (result i64)))
        (memory (export "memory") 4 8)
        (global $key (mut i32) (i32.const 0))
        (global $ver (mut i32) (i32.const 0))
        (data (i32.const 1024) "{empty_esc}")
        (data (i32.const 1100) "{ctx_esc}")
        (data (i32.const 1140) "{req_esc}")
        (data (i32.const 1180) "{seg_esc}")
        (data (i32.const 1220) "{hang_esc}")
        (data (i32.const 1260) "{off_esc}")
        (data (i32.const 1300) "{len_esc}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        {WAT_FIND_FN}
        (func $parse_u64 (param $p i32) (param $end i32) (result i64)
            (local $acc i64)
            (local $c i32)
            (block $done
                (loop $loop
                    (br_if $done (i32.ge_u (local.get $p) (local.get $end)))
                    (local.set $c (i32.load8_u (local.get $p)))
                    (br_if $done (i32.lt_u (local.get $c) (i32.const 48)))
                    (br_if $done (i32.gt_u (local.get $c) (i32.const 57)))
                    (local.set $acc
                        (i64.add
                            (i64.mul (local.get $acc) (i64.const 10))
                            (i64.extend_i32_u (i32.sub (local.get $c) (i32.const 48)))))
                    (local.set $p (i32.add (local.get $p) (i32.const 1)))
                    (br $loop)))
            (local.get $acc))
        (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
            (local $pos i32)
            (local $end i32)
            (local $id i64)
            (local $seg_tag i32)
            (local $off i32)
            (local $req_len i32)
            (local $take i32)
            (local $eof i32)
            (local $i i32)
            (local $blob i64)
            (local.set $end (i32.add (local.get $ptr) (local.get $len)))
            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1100) (i32.const {ctx_len})))
            (if (i32.ge_s (local.get $pos) (i32.const 0))
                (then
                    (local.set $id (call $parse_u64 (local.get $pos) (local.get $end)))
                    (if (i64.eq (call $blob_len (local.get $id)) (i64.const 2))
                        (then
                            (drop (call $blob_read (local.get $id) (i64.const 0) (i32.const 512) (i32.const 2)))
                            (global.set $key (i32.load8_u (i32.const 512)))
                            (global.set $ver (i32.load8_u (i32.const 513)))))
                    (drop (call $host_close (local.get $id)))
                    (return (i64.const {empty_packed}))))
            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1140) (i32.const {req_len})))
            (if (i32.ge_s (local.get $pos) (i32.const 0))
                (then
                    (local.set $id (call $parse_u64 (local.get $pos) (local.get $end)))
                    (if (i32.ge_s (call $find (local.get $ptr) (local.get $len) (i32.const 1220) (i32.const {hang_len})) (i32.const 0))
                        (then (return (i64.const {empty_packed}))))
                    (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1180) (i32.const {seg_len})))
                    (if (i32.ge_s (local.get $pos) (i32.const 0))
                        (then
                            (local.set $seg_tag (i32.load8_u (local.get $pos)))
                            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1260) (i32.const {off_len})))
                            (local.set $off (i32.wrap_i64 (call $parse_u64 (local.get $pos) (local.get $end))))
                            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1300) (i32.const {len_len})))
                            (local.set $req_len (i32.wrap_i64 (call $parse_u64 (local.get $pos) (local.get $end))))
                            (local.set $take (i32.sub (i32.const 32) (local.get $off)))
                            (if (i32.gt_u (local.get $take) (local.get $req_len))
                                (then (local.set $take (local.get $req_len))))
                            (local.set $eof (i32.ge_u (i32.add (local.get $off) (local.get $take)) (i32.const 32)))
                            (local.set $i (i32.const 0))
                            (block $fill_done
                                (loop $fill
                                    (br_if $fill_done (i32.ge_u (local.get $i) (local.get $take)))
                                    (i32.store8
                                        (i32.add (i32.const 4096) (local.get $i))
                                        (i32.xor
                                            (i32.xor
                                                (i32.add (i32.add (local.get $off) (local.get $i)) (local.get $seg_tag))
                                                (global.get $key))
                                            (global.get $ver)))
                                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                                    (br $fill)))
                            (local.set $blob (call $blob_new (i32.const 4096) (local.get $take)))
                            (drop (call $stream_reply (local.get $id) (local.get $blob) (local.get $eof)))))))
            (i64.const {empty_packed})))"#,
        empty_esc = esc(empty),
        ctx_esc = esc(ctx_needle),
        req_esc = esc(req_needle),
        seg_esc = esc(seg_needle),
        hang_esc = esc(hang_needle),
        off_esc = esc(off_needle),
        len_esc = esc(len_needle),
        ctx_len = ctx_needle.len(),
        req_len = req_needle.len(),
        seg_len = seg_needle.len(),
        hang_len = hang_needle.len(),
        off_len = off_needle.len(),
        len_len = len_needle.len(),
    ))
    .unwrap()
}

struct NoOpExecutor;
impl Executor for NoOpExecutor {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        _: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}

#[test]
fn a09_guest_virtual_hls_rolling_context_evolution_rollback_rejection_control_fault_and_ledger_zero()
 {
    let engine = RuntimeEngine::new().unwrap();
    let stream_module = engine
        .compile(&build_a09_stream_guest_wasm(), &CancelToken::new())
        .unwrap();

    // Compile a Control actor that emits an unauthorized command (`show_toast` without grant)
    // to prove Control-role failures never tear down or corrupt an active StreamSession/Relay.
    let bad_ctrl_reply = r#"{"ok":[{"command":"show_toast","message":"unauthorized"}]}"#;
    let bad_ctrl_esc: String = bad_ctrl_reply
        .as_bytes()
        .iter()
        .map(|b| format!("\\{b:02x}"))
        .collect();
    let bad_ctrl_packed = (2048_u64 << 32) | bad_ctrl_reply.len() as u64;
    let ctrl_wasm = wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 2 4)
        (data (i32.const 2048) "{bad_ctrl_esc}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 16384)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const {bad_ctrl_packed}))"#
    ))
    .unwrap();
    let ctrl_module = engine.compile(&ctrl_wasm, &CancelToken::new()).unwrap();

    let (factory, grant, ledger) = factory();
    let io = IoPool::new().unwrap();
    let origin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    origin_listener.set_nonblocking(true).unwrap();
    let origin_addr = origin_listener.local_addr().unwrap();
    let stop_origin = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_origin_bg = Arc::clone(&stop_origin);
    let origin_thread = std::thread::spawn(move || {
        while !stop_origin_bg.load(Ordering::Relaxed) {
            let Ok((mut sock, _)) = origin_listener.accept() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            sock.set_nonblocking(false).unwrap();
            let req = headers(&mut sock);
            let target = req
                .lines()
                .next()
                .and_then(|l| l.split_ascii_whitespace().nth(1))
                .unwrap_or("");
            let ver: u8 = target
                .strip_prefix("/key_v")
                .and_then(|s| s.strip_suffix(".key"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let key_bytes = [ver ^ 0xa5; 16];
            sock.write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-15/16\r\nContent-Length: 16\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            sock.write_all(&key_bytes).unwrap();
        }
    });

    let key_origin = grant
        .confirm_origin(
            &format!("http://{origin_addr}/key_v1.key"),
            TransportKind::Http,
            &["127.0.0.1".parse().unwrap()],
        )
        .unwrap();

    let now_ms = || -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };

    let sess_id = new_session_id().unwrap();
    let initial_ctx = StreamContext {
        version: Id(1),
        blob: Id(1),
        origin_grants: vec![key_origin],
        transports: vec![Capability::NetHttp],
        expires_unix_ms: Some(WireU64(now_ms() + 60_000)),
    };
    let store = Arc::new(
        StreamStore::spawn(
            Spawn {
                engine: engine.clone(),
                module: stream_module,
                role: Role::Stream,
                owner: "stream-tests".into(),
                grants: BTreeSet::from([Capability::NetHttp]),
                ledger: ledger.clone(),
                io: io.clone(),
                executor: Arc::new(NoOpExecutor),
                generation: Generation::default(),
                wake: Arc::new(|| {}),
            },
            sess_id,
            &initial_ctx,
            Arc::new(Blob::new(&ledger, "stream-tests", vec![0x5a, 1]).unwrap()),
        )
        .unwrap(),
    );

    let bound = BoundRelay::bind().unwrap();
    let sess = StreamSession::new(
        sess_id,
        Generation {
            media: Id(0),
            seek: Id(0),
            context: Id(1),
        },
        bound.authority().unwrap(),
        factory,
        Some(Arc::clone(&store)),
    )
    .unwrap();
    sess.enable_guest_manifest_updates();
    assert!(sess.allows_guest_manifest_updates());

    let make_manifest = |round: usize, key_ver: u8| -> Vec<u8> {
        let key_req = RemoteResource {
            url: format!("http://{origin_addr}/key_v{key_ver}.key"),
            headers: Vec::new(),
            origin_grant: Some(key_origin),
        };
        let mut resources = vec![json!({
            "id": "k_cur",
            "kind": "key",
            "extension": "key",
            "size": "16",
            "source": {"transport": "remote", "request": key_req}
        })];
        let mut segments = Vec::new();
        for idx in round..(round + 3) {
            let id = format!("seg_r{idx}");
            resources.push(json!({
                "id": id,
                "kind": "media",
                "extension": "ts",
                "size": "32",
                "source": {"transport": "guest", "resource_id": id}
            }));
            segments.push(json!({
                "resource": {"resource_id": id, "range": null},
                "duration_ms": "1000",
                "init": null,
                "key": {"resource_id": "k_cur", "iv": format!("{:032x}", 500 + idx)}
            }));
        }
        // Include "hang" in round 0 only if referenced; keep separate test for in-flight seek cancel via store.reader.
        serde_json::to_vec(&json!({
            "resources": resources,
            "tracks": [{
                "id": "main",
                "kind": "video",
                "label": "Live Video",
                "lang": null,
                "codec": null,
                "default": true,
                "sequence": (500 + round).to_string(),
                "segments": segments
            }],
            "variants": [],
            "live": true
        }))
        .unwrap()
    };

    let doc0 = make_manifest(0, 1);
    sess.commit(
        sess.prepare_manifest(Id(0), ManifestFormat::VirtualHls, &doc0)
            .unwrap(),
    )
    .unwrap();

    // Attach external audio & subtitle tracks; both must survive every rolling commit & context update.
    let audio_attach = sess
        .add_attachment(
            "ext_audio",
            "bin",
            Arc::new(MemoryReader::new(b"AAC-TRACK".to_vec(), "stream-tests", &ledger).unwrap()),
        )
        .unwrap();
    let sub_attach = sess
        .add_attachment(
            "ext_sub",
            "srt",
            Arc::new(
                MemoryReader::new(
                    b"1\n00:00:00,000 --> 00:00:02,000\nSub\n".to_vec(),
                    "stream-tests",
                    &ledger,
                )
                .unwrap(),
            ),
        )
        .unwrap();

    let relay = bound.start(sess.clone()).unwrap();
    let root_url = sess.root_url().unwrap();

    // Trigger a failing Control actor on the same runtime/ledger mid-playback.
    let ctrl_actor = Actor::spawn(Spawn {
        engine,
        module: ctrl_module,
        role: Role::Control,
        owner: "stream-tests".into(),
        grants: BTreeSet::new(),
        ledger: ledger.clone(),
        io,
        executor: Arc::new(NoOpExecutor),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    ctrl_actor
        .send(
            serde_json::to_vec(&json!({
                "event": "init",
                "protocol": 2,
                "abi": 2,
                "host_version": "0.0.2",
                "role": "control",
                "generation": Generation::default(),
                "capabilities": [],
                "features": [],
                "snapshot": null
            }))
            .unwrap(),
        )
        .unwrap();
    until(|| ctrl_actor.status() == Status::Failed(ErrorCode::Denied));

    let versions: [(u64, u8); 6] = [
        (1, 0x5a),
        (2, 0x33),
        (4, 0x9c),
        (7, 0x6e),
        (11, 0xa7),
        (16, 0xd4),
    ];
    let mut cur_ver = 1u64;
    let mut cur_key = 0x5au8;
    let mut revoked_urls: Vec<String> = Vec::new();
    let mut prev_segs: Option<Vec<String>> = None;
    let mut prev_key_url: Option<String> = None;

    for (round, &(target_ver, target_key)) in versions.iter().enumerate() {
        if round > 0 {
            // 1. Reject stale expected version, equal/rollback version, authority broadening, and past expiry.
            let valid_next = StreamContext {
                version: Id(target_ver),
                blob: Id(1),
                origin_grants: vec![key_origin],
                transports: vec![Capability::NetHttp],
                expires_unix_ms: Some(WireU64(now_ms() + 60_000)),
            };
            assert_eq!(
                sess.refresh_context(
                    Id(cur_ver + 99),
                    &valid_next,
                    Arc::new(Blob::new(&ledger, "stream-tests", vec![0x11, 99]).unwrap()),
                ),
                Err(ErrorCode::StaleHandle),
                "mismatched expected version must fail"
            );
            for bad_ver in [cur_ver, cur_ver - 1] {
                if bad_ver == 0 {
                    continue;
                }
                let rollback = StreamContext {
                    version: Id(bad_ver),
                    ..valid_next.clone()
                };
                assert_eq!(
                    sess.refresh_context(
                        Id(cur_ver),
                        &rollback,
                        Arc::new(Blob::new(&ledger, "stream-tests", vec![0x11, 88]).unwrap()),
                    ),
                    Err(ErrorCode::StaleHandle),
                    "non-monotonic context version {bad_ver} (cur={cur_ver}) must be rejected"
                );
            }
            let broadened = StreamContext {
                transports: vec![Capability::NetHttp, Capability::NetWs],
                ..valid_next.clone()
            };
            assert_eq!(
                sess.refresh_context(
                    Id(cur_ver),
                    &broadened,
                    Arc::new(Blob::new(&ledger, "stream-tests", vec![0x11, 77]).unwrap()),
                ),
                Err(ErrorCode::Denied),
                "broadening transports during refresh must be denied"
            );
            let expired = StreamContext {
                expires_unix_ms: Some(WireU64(now_ms().saturating_sub(1))),
                ..valid_next.clone()
            };
            assert_eq!(
                sess.refresh_context(
                    Id(cur_ver),
                    &expired,
                    Arc::new(Blob::new(&ledger, "stream-tests", vec![0x11, 66]).unwrap()),
                ),
                Err(ErrorCode::StaleHandle),
                "already-expired context must be rejected"
            );

            // Active valid context must STILL serve reads cleanly after all rejected updates!
            let current_pl = std::str::from_utf8(split_http(&request(&root_url, "GET", "")).1)
                .unwrap()
                .to_owned();
            let active_seg = current_pl
                .lines()
                .find(|l| l.starts_with("http://"))
                .unwrap();
            let prev_tag = b'0' + (round - 1) as u8;
            let expected_active: Vec<u8> = (0u8..32)
                .map(|i| i.wrapping_add(prev_tag) ^ cur_key ^ (cur_ver as u8))
                .collect();
            assert_eq!(
                split_http(&request(active_seg, "GET", "")).1,
                expected_active.as_slice()
            );

            // 2. Apply valid monotonic context rotation + slide VirtualHls window.
            sess.refresh_context(
                Id(cur_ver),
                &valid_next,
                Arc::new(
                    Blob::new(&ledger, "stream-tests", vec![target_key, target_ver as u8]).unwrap(),
                ),
            )
            .unwrap();
            cur_ver = target_ver;
            cur_key = target_key;
            assert_eq!(sess.generation().context, Id(cur_ver));

            // Stale manifest revision must be rejected.
            let next_doc = make_manifest(round, (round + 1) as u8);
            assert!(matches!(
                sess.prepare_manifest(
                    Id((round - 1) as u64),
                    ManifestFormat::VirtualHls,
                    &next_doc
                ),
                Err(ErrorCode::StaleHandle)
            ));
            let prepared = sess
                .prepare_manifest(Id(round as u64), ManifestFormat::VirtualHls, &next_doc)
                .unwrap();
            sess.commit(prepared).unwrap();
        }

        let pl_raw = request(&root_url, "GET", "");
        let (pl_head, pl_body) = split_http(&pl_raw);
        assert!(pl_head.starts_with("HTTP/1.1 200 OK\r\n"));
        let pl_text = std::str::from_utf8(pl_body).unwrap();
        assert!(pl_text.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", 500 + round)));

        let key_url = pl_text
            .lines()
            .find_map(|l| l.split("URI=\"").nth(1)?.split('"').next())
            .unwrap()
            .to_owned();
        let seg_urls: Vec<String> = pl_text
            .lines()
            .filter(|l| l.starts_with("http://"))
            .map(str::to_owned)
            .collect();
        assert_eq!(seg_urls.len(), 3);

        // Verify rotated remote key bytes.
        assert_eq!(
            split_http(&request(&key_url, "GET", "")).1,
            &[((round + 1) as u8) ^ 0xa5; 16]
        );

        // Verify all 3 segments in the active window (full GET and partial 206 Range!) match
        // the newly rotated StreamContext key & version.
        for (slot, seg_url) in seg_urls.iter().enumerate() {
            let seg_idx = round + slot;
            let seg_tag = b'0' + seg_idx as u8;
            let expected: Vec<u8> = (0u8..32)
                .map(|i| i.wrapping_add(seg_tag) ^ cur_key ^ (cur_ver as u8))
                .collect();
            let raw_full = request(seg_url, "GET", "");
            let (h_full, b_full) = split_http(&raw_full);
            assert!(h_full.starts_with("HTTP/1.1 200 OK\r\n"));
            assert_eq!(b_full, expected.as_slice());

            let raw_part = request(seg_url, "GET", "Range: bytes=8-23\r\n");
            let (h_part, b_part) = split_http(&raw_part);
            assert!(h_part.starts_with("HTTP/1.1 206 Partial Content\r\n"));
            assert_eq!(b_part, &expected[8..24]);
        }

        if let Some(old_segs) = prev_segs.replace(seg_urls.clone()) {
            // `old_segs[0]` rolled out of the window; `old_segs[1..3]` stayed in the window with stable tokens!
            assert_eq!(old_segs[1], seg_urls[0]);
            assert_eq!(old_segs[2], seg_urls[1]);
            revoked_urls.push(old_segs[0].clone());
        }
        if let Some(old_key) = prev_key_url.replace(key_url) {
            revoked_urls.push(old_key);
        }

        for revoked in &revoked_urls {
            let resp = request(revoked, "GET", "");
            assert!(
                resp.starts_with(b"HTTP/1.1 410 Error\r\n"),
                "rolled-out capability {revoked} was not revoked in round {round}: {:?}",
                String::from_utf8_lossy(&resp)
            );
        }

        // External attachments survive every round untouched.
        assert_eq!(
            split_http(&request(&audio_attach, "GET", "")).1,
            b"AAC-TRACK"
        );
        assert!(
            split_http(&request(&sub_attach, "GET", ""))
                .1
                .ends_with(b"Sub\n")
        );
    }

    // 3. In-flight guest read cancellation on `advance_seek()` and context expiry enforcement.
    let hang_reader = store.reader("hang".into(), Some(32)).unwrap();
    let seek_cancel = HttpCancel::default();
    let hang_cancel = seek_cancel.clone();
    let hang_handle = std::thread::spawn(move || hang_reader.read_at(0, 32, &hang_cancel).err());
    until(|| ledger.used(None, Resource::StreamRead) == 1);
    let advanced = sess.advance_seek().unwrap();
    assert_eq!(advanced.seek, Id(1));
    assert_eq!(hang_handle.join().unwrap(), Some(ErrorCode::Cancelled));

    // Refresh with short TTL and verify expiry immediately blocks new reads over Relay (410 StaleHandle).
    let short_ttl_ctx = StreamContext {
        version: Id(20),
        blob: Id(1),
        origin_grants: vec![key_origin],
        transports: vec![Capability::NetHttp],
        expires_unix_ms: Some(WireU64(now_ms() + 80)),
    };
    sess.refresh_context(
        Id(cur_ver),
        &short_ttl_ctx,
        Arc::new(Blob::new(&ledger, "stream-tests", vec![0x7f, 20]).unwrap()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    let final_pl = std::str::from_utf8(split_http(&request(&root_url, "GET", "")).1)
        .unwrap()
        .to_owned();
    let final_seg = final_pl.lines().find(|l| l.starts_with("http://")).unwrap();
    let expired_read = request(final_seg, "GET", "");
    assert!(
        expired_read.starts_with(b"HTTP/1.1 410 Error\r\n"),
        "expired StreamContext must block new guest reads with 410, got {:?}",
        String::from_utf8_lossy(&expired_read)
    );

    // 4. Clean teardown & full Ledger zeroing across all 21 resource kinds.
    ctrl_actor.stop();
    relay.stop();
    until(|| relay.is_quiescent() && store.is_quiescent() && ctrl_actor.is_quiescent());
    stop_origin.store(true, Ordering::Relaxed);
    origin_thread.join().unwrap();
    drop((ctrl_actor, relay, sess, store, grant));
    assert_ledger_zero(&ledger);
}
