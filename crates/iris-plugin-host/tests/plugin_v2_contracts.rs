//! Black-box protocol-2 host contract tests exercising [`PluginHost`] and
//! [`WindowSession`] strictly through the public root facade.
//! Run `scripts/test-plugin-v2.py` to execute the compiled Rust guest cases.

use iris_api::v2::{Capability, ErrorCode, HostEvent, Id, source::PlaybackSnapshot};
use iris_core::vault::CredentialVault;
use iris_plugin_host::{ActionKind, Notice, PluginHost, PluginStatus, WindowSession};
use std::{
    io::{Cursor, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use zip::{ZipWriter, write::SimpleFileOptions};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[track_caller]
fn until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let end = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn guest(name: &str) -> Vec<u8> {
    std::fs::read(
        std::env::var(name).expect("run scripts/test-plugin-v2.py to compile language fixtures"),
    )
    .unwrap()
}

fn write_plugin_zip(path: &Path, manifest: &serde_json::Value, wasm: &[u8]) {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("plugin.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&serde_json::to_vec(manifest).unwrap())
        .unwrap();
    zip.start_file("plugin.wasm", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(wasm).unwrap();
    let archive = zip.finish().unwrap().into_inner();
    std::fs::write(path, archive).unwrap();
}

fn is_synced_ready(session: &mut WindowSession, plugin_id: &str) -> bool {
    let ready = session.is_ready(plugin_id);
    session.poll();
    ready
        && session
            .list()
            .iter()
            .any(|p| p.id == plugin_id && p.status == PluginStatus::Ready)
}

fn install_and_wait_ready(session: &mut WindowSession, pkg_path: &Path, plugin_id: &str) {
    let review_id = session.review_package(pkg_path.to_path_buf()).unwrap();
    let mut review = None;
    until(
        || {
            for notice in session.poll().notices {
                match notice {
                    Notice::Review(r) if r.id == review_id => review = Some(r),
                    Notice::Error { code, .. } => panic!("review failed: {code:?}"),
                    _ => {}
                }
            }
            review.is_some()
        },
        Duration::from_secs(60),
    );
    let summary = review.unwrap();
    let grants = summary.manifest.permissions.iter().copied().collect();
    session.approve_review(review_id, grants, false).unwrap();
    until(
        || is_synced_ready(session, plugin_id),
        Duration::from_secs(60),
    );
}

fn read_loopback_range(url_str: &str, start: u64, len: u64) -> Result<Vec<u8>, std::io::Error> {
    let parsed = url::Url::parse(url_str).unwrap();
    let authority = format!(
        "{}:{}",
        parsed.host_str().unwrap(),
        parsed.port_or_known_default().unwrap()
    );
    let mut stream = TcpStream::connect(&authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let end = start + len - 1;
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes={start}-{end}\r\nConnection: close\r\n\r\n",
        parsed.path()
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    if !raw.starts_with(b"HTTP/1.1 206 ") {
        return Err(std::io::Error::other(format!(
            "unexpected HTTP status: {}",
            String::from_utf8_lossy(&raw[..raw.len().min(64)])
        )));
    }
    let body_start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("missing header separator"))?
        + 4;
    Ok(raw[body_start..].to_vec())
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

#[test]
fn wat_ws_request_origin_returns_notice_without_quarantining_plugin() {
    let empty = r#"{"ok":[]}"#;
    let trigger = r#""connect""#;
    let esc = |s: &str| -> String { s.as_bytes().iter().map(|b| format!("\\{b:02x}")).collect() };
    let empty_packed = (1024_u64 << 32) | empty.len() as u64;
    let reply_json = r#"{"ok":[{"command":"request_origin","request_id":"1","origin":"ws://192.168.1.50:8080/chat"}]}"#;
    let reply_packed = (2048_u64 << 32) | reply_json.len() as u64;

    let wasm = wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 4 8)
        (data (i32.const 1024) "{empty_esc}")
        (data (i32.const 1100) "{trigger_esc}")
        (data (i32.const 2048) "{reply_esc}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        {WAT_FIND_FN}
        (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
            (if (result i64) (i32.ge_s (call $find (local.get $ptr) (local.get $len) (i32.const 1100) (i32.const {trigger_len})) (i32.const 0))
                (then (i64.const {reply_packed}))
                (else (i64.const {empty_packed})))))"#,
        empty_esc = esc(empty),
        trigger_esc = esc(trigger),
        reply_esc = esc(reply_json),
        trigger_len = trigger.len(),
    ))
    .unwrap();

    let plugin_id = "wat.origin.ws";
    let dir = TempDir::new("iris-wat-ws-origin");
    let pkg_path = dir.path().join("ws-origin.irisplugin");
    let manifest = serde_json::json!({
        "id": plugin_id,
        "name": "WAT WS Origin Fixture",
        "version": "1.0.0",
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": [Capability::UiModal, Capability::NetWs],
    });
    write_plugin_zip(&pkg_path, &manifest, &wasm);

    let host = PluginHost::open_in(dir.path(), CredentialVault::in_memory(), &[], false).unwrap();
    let (mut session, _) = host.open_window(&[], "dark");
    install_and_wait_ready(&mut session, &pkg_path, plugin_id);

    let initial = session.send_to(
        plugin_id,
        HostEvent::Command {
            id: "connect".into(),
            snapshot: None,
        },
    );
    assert!(initial.notices.is_empty());

    let mut summary = None;
    until(
        || {
            for notice in session.poll().notices {
                match notice {
                    Notice::Origin(s) => summary = Some(s),
                    Notice::Error { code, .. } => {
                        panic!("unexpected host error for ws:// RequestOrigin: {code:?}");
                    }
                    _ => {}
                }
            }
            for plugin in session.list() {
                if plugin.id == plugin_id {
                    assert!(
                        !matches!(plugin.status, PluginStatus::Failed(_)),
                        "ws:// RequestOrigin quarantined plugin: {:?}",
                        plugin.status
                    );
                }
            }
            summary.is_some()
        },
        Duration::from_secs(20),
    );

    let summary = summary.unwrap();
    assert_eq!(summary.origin, "http://192.168.1.50:8080");
    assert_eq!(
        summary.ips,
        vec!["192.168.1.50".parse::<std::net::IpAddr>().unwrap()]
    );
    assert!(summary.private && summary.plaintext);

    let origin_id = summary.id;
    drop(summary);
    session.confirm_origin(origin_id, true, false).unwrap();
    let mut closed = false;
    until(
        || {
            for notice in session.poll().notices {
                if let Notice::OriginClosed(id) = notice
                    && id == origin_id
                {
                    closed = true;
                }
            }
            closed
        },
        Duration::from_secs(20),
    );
    assert!(session.is_ready(plugin_id));

    session.shutdown();
    drop(session);
    host.shutdown();
    until(|| host.host_bytes_used() == 0, Duration::from_secs(20));
}

fn send_runtime_command(session: &mut WindowSession, command: &str) -> String {
    let report = session.send_to(
        "test.runtime",
        HostEvent::Command {
            id: command.into(),
            snapshot: Some(PlaybackSnapshot {
                uri: Some("file:///must-remain-private.mov".into()),
                ..PlaybackSnapshot::default()
            }),
        },
    );
    assert!(report.notices.is_empty());
    let mut toast = None;
    until(
        || {
            for action in session.poll().actions {
                if let ActionKind::ShowToast { message } = action.kind {
                    toast = Some(message);
                }
            }
            toast.is_some()
        },
        Duration::from_secs(20),
    );
    toast.unwrap()
}

fn setup_runtime_package(dir: &TempDir) -> PathBuf {
    let pkg_path = dir.path().join("test.runtime.irisplugin");
    let manifest = serde_json::json!({
        "id": "test.runtime",
        "name": "Rust runtime",
        "version": "1",
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": ["notifications", "media_source"]
    });
    write_plugin_zip(&pkg_path, &manifest, &guest("IRIS_V2_RUST_GUEST"));
    pkg_path
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn installed_guest_persists_state_without_read_state_and_cannot_receive_local_snapshot() {
    let dir = TempDir::new("iris-runtime-guest");
    let pkg_path = setup_runtime_package(&dir);
    let vault = CredentialVault::in_memory();

    let host = PluginHost::open_in(dir.path(), vault.clone(), &[], false).unwrap();
    let (mut session, _) = host.open_window(&[], "dark");
    install_and_wait_ready(&mut session, &pkg_path, "test.runtime");
    assert_eq!(send_runtime_command(&mut session, "snapshot"), "none");
    assert_eq!(send_runtime_command(&mut session, "store_read"), "absent");
    assert_eq!(
        send_runtime_command(&mut session, "store_write"),
        "persistent-value"
    );
    session.shutdown();
    drop(session);
    host.shutdown();
    until(
        || host.host_bytes_used() == 0 && host.control_stores_used() == 0,
        Duration::from_secs(20),
    );
    drop(host);

    let reopened_host = PluginHost::open_in(dir.path(), vault, &[], false).unwrap();
    let (mut reopened, _) = reopened_host.open_window(&[], "dark");
    until(
        || is_synced_ready(&mut reopened, "test.runtime"),
        Duration::from_secs(60),
    );
    assert_eq!(
        send_runtime_command(&mut reopened, "store_read"),
        "persistent-value"
    );
    reopened.shutdown();
    drop(reopened);
    reopened_host.shutdown();
    until(
        || reopened_host.host_bytes_used() == 0 && reopened_host.control_stores_used() == 0,
        Duration::from_secs(20),
    );
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn admitted_playback_survives_request_cancel_and_control_timeout_until_explicit_stop() {
    let dir = TempDir::new("iris-runtime-survive");
    let pkg_path = setup_runtime_package(&dir);
    let host = PluginHost::open_in(dir.path(), CredentialVault::in_memory(), &[], false).unwrap();
    let (mut session, _) = host.open_window(&[], "dark");
    install_and_wait_ready(&mut session, &pkg_path, "test.runtime");

    let req = session
        .resolve("test.runtime", "fixture.source", None, "av", None)
        .unwrap();
    let mut playback_notice = None;
    until(
        || {
            for notice in session.poll().notices {
                match notice {
                    Notice::Playback {
                        ticket, playback, ..
                    } if ticket.id == req => {
                        playback_notice = Some((ticket, playback));
                    }
                    Notice::Error { code, .. } => panic!("resolve failed: {code:?}"),
                    _ => {}
                }
            }
            playback_notice.is_some()
        },
        Duration::from_secs(60),
    );
    let (ticket, playback) = playback_notice.unwrap();
    let url = playback.url.clone();
    // Preparation cancellation scope ends once admitted; cancelling `ticket.cancel` does not kill the stream.
    ticket.cancel.cancel();
    drop(ticket);

    assert_eq!(
        read_loopback_range(&url, 5, 16).unwrap(),
        (5..21).map(|n| (n as u8) ^ 0xa5).collect::<Vec<_>>()
    );

    // Trigger a 2s dead-loop timeout in ControlStore; admitted StreamStore + Relay must survive!
    let _ = session.send_to(
        "test.runtime",
        HostEvent::Command {
            id: "spin".into(),
            snapshot: None,
        },
    );
    until(
        || {
            session.poll();
            session.list().iter().any(|p| {
                p.id == "test.runtime" && p.status == PluginStatus::Failed(ErrorCode::Timeout)
            })
        },
        Duration::from_secs(20),
    );

    assert_eq!(
        read_loopback_range(&url, 40, 16).unwrap(),
        (40..56).map(|n| (n as u8) ^ 0xa5).collect::<Vec<_>>()
    );
    assert_eq!(
        session.precheck_capability("test.runtime", Capability::MediaSource),
        Err(ErrorCode::Timeout)
    );

    // Explicit window/host teardown cancels the relay.
    session.shutdown();
    drop((playback, session));
    host.shutdown();
    until(
        || read_loopback_range(&url, 0, 16).is_err(),
        Duration::from_secs(20),
    );
    until(|| host.is_ledger_empty(), Duration::from_secs(20));
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn coordinator_routes_live_source_requests_and_stops_stream_on_window_close() {
    let dir = TempDir::new("iris-runtime-coord");
    let pkg_path = setup_runtime_package(&dir);
    let host = PluginHost::open_in(dir.path(), CredentialVault::in_memory(), &[], false).unwrap();
    let (mut first, _) = host.open_window(&[], "dark");
    let (mut second, _) = host.open_window(&[], "dark");
    assert!(!second.is_connected());
    assert!(matches!(
        second.poll().notices.as_slice(),
        [Notice::Error {
            code: ErrorCode::Backpressure,
            ..
        }]
    ));
    drop(second);

    install_and_wait_ready(&mut first, &pkg_path, "test.runtime");
    assert_eq!(host.control_stores_used(), 1);
    assert!(
        first
            .media_sources()
            .iter()
            .any(|(pid, _, src)| *pid == "test.runtime" && src.id == "fixture.source")
    );

    // A UI-supplied SourceResolve event cannot bypass the coordinator's source request table.
    let spoofed = first.send_to(
        "test.runtime",
        HostEvent::SourceResolve {
            request_id: Id(999),
            source_id: "fixture.source".into(),
            folder_id: None,
            entry_id: "av".into(),
            variant: None,
        },
    );
    assert!(matches!(
        spoofed.notices.as_slice(),
        [Notice::Error {
            code: ErrorCode::Denied,
            ..
        }]
    ));

    let request = first
        .resolve("test.runtime", "fixture.source", None, "av", None)
        .unwrap();
    let mut playback = None;
    until(
        || {
            for notice in first.poll().notices {
                match notice {
                    Notice::Playback {
                        ticket,
                        playback: ready,
                        ..
                    } => {
                        assert_eq!(ticket.id, request);
                        playback = Some(ready);
                    }
                    Notice::Error { code, .. } => panic!("source preparation: {code:?}"),
                    _ => {}
                }
            }
            playback.is_some()
        },
        Duration::from_secs(60),
    );
    let playback = playback.unwrap();
    let url = playback.url.clone();
    assert_eq!(
        read_loopback_range(&url, 5, 16).unwrap(),
        (5..21).map(|n| (n as u8) ^ 0xa5).collect::<Vec<_>>()
    );

    first.shutdown();
    until(
        || read_loopback_range(&url, 5, 16).is_err(),
        Duration::from_secs(20),
    );
    drop((playback, first));
    host.shutdown();
    until(|| host.host_bytes_used() == 0, Duration::from_secs(20));
}
