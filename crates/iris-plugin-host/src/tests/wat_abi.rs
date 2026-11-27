//! Default-enabled pure-WAT white-box ABI v2 invariants exercising internal
//! `Instance`, `Control`, and `StreamStore` state machines directly.

use crate::{
    actor::{Executor, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    installation::{Approval, Installed, Installer},
    instance::{Blob, Instance, InstanceConfig},
    ledger::{Ledger, Resource},
    package::{Package, Provenance},
    pool::IoPool,
    runtime::Control,
    services::{Resources, Setup},
    stream::{guest::StreamStore, session::new_session_id},
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostCommand, HostEvent, Id, MESSAGE_LIMIT,
    NetworkDeclaration, Role, stream::StreamContext, task::Task,
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::CancelToken;
use iris_core::logging::{self, TelemetryEvent, TelemetrySink};
use iris_core::vault::CredentialVault;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zip::{ZipWriter, write::SimpleFileOptions};

fn until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let end = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wat_reply_wasm(reply_json: &str, init_empty: bool) -> Vec<u8> {
    let empty = r#"{"ok":[]}"#;
    let empty_escaped: String = empty
        .as_bytes()
        .iter()
        .map(|b| format!("\\{b:02x}"))
        .collect();
    let empty_packed = (1024_u64 << 32) | empty.len() as u64;

    let escaped: String = reply_json
        .as_bytes()
        .iter()
        .map(|b| format!("\\{b:02x}"))
        .collect();
    let packed = (2048_u64 << 32) | reply_json.len() as u64;
    let body = if init_empty {
        format!(
            "(if (result i64) (i32.eqz (global.get $seen))
                (then (global.set $seen (i32.const 1)) (i64.const {empty_packed}))
                (else (i64.const {packed})))"
        )
    } else {
        format!("i64.const {packed}")
    };
    wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 4 8)
        (global $seen (mut i32) (i32.const 0))
        (data (i32.const 1024) "{empty_escaped}")
        (data (i32.const 2048) "{escaped}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) {body}))"#
    ))
    .unwrap()
}

struct WatFixture {
    root: PathBuf,
    installer: Installer,
    installed: Installed,
    setup: Setup,
}

impl WatFixture {
    fn new(plugin_id: &str, reply_json: &str, grants: Vec<Capability>, init_empty: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-wat-abi-{}-{}-{}",
            plugin_id.replace('.', "_"),
            std::process::id(),
            Control::new_window_id().unwrap().0
        ));
        let resources = Resources::new().unwrap();
        let setup = Setup {
            resources: resources.clone(),
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        };
        let manifest = serde_json::json!({
            "id": plugin_id,
            "name": "WAT Contract Fixture",
            "version": "1.0.0",
            "protocol": 2,
            "abi": 2,
            "entry": "plugin.wasm",
            "permissions": grants,
        });
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in [
            ("plugin.json", serde_json::to_vec(&manifest).unwrap()),
            ("plugin.wasm", wat_reply_wasm(reply_json, init_empty)),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(&bytes).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let digest = Package::from_zip(&bytes, Provenance::User).unwrap().digest;
        let mut installer = Installer::new(root.join("installed"), resources.ledger.clone());
        let installed = installer
            .install(
                &bytes,
                Approval {
                    digest,
                    grants: grants.into_iter().collect(),
                    network: NetworkDeclaration::default(),
                    previous: None,
                },
            )
            .unwrap();
        Self {
            root,
            installer,
            installed,
            setup,
        }
    }

    fn open_window(&self, control: &mut Control, win: Id) {
        let prepared = control
            .window_preparation(
                win,
                Generation::default(),
                None,
                Arc::new(|_, _, _| Err(ErrorCode::Denied)),
            )
            .unwrap()
            .prepare()
            .unwrap();
        control.attach_window(prepared).unwrap();
    }

    fn load(&self) -> Control {
        Control::load(
            &self.installer,
            self.installed.clone(),
            self.setup.clone(),
            &CancelToken::new(),
        )
        .unwrap()
    }
}

impl Drop for WatFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn wat_abi_v2_handshake_and_return_pointer_bounds() {
    let engine = RuntimeEngine::new().unwrap();
    let cancel = CancelToken::new();
    let new_inst = |source: &str| {
        let wasm = wat::parse_str(source).unwrap();
        let module = engine.compile(&wasm, &cancel).unwrap();
        Instance::new(InstanceConfig {
            engine: engine.clone(),
            module,
            role: Role::Control,
            owner: "wat-abi".into(),
            grants: BTreeSet::new(),
            ledger: Ledger::default(),
            cancel: cancel.clone(),
        })
    };

    let bad_abi = r#"(module
        (memory (export "memory") 1 2)
        (func (export "iris_abi_version") (result i32) i32.const 1)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#;
    assert!(new_inst(bad_abi).is_err());

    let missing_export = r#"(module
        (memory (export "memory") 1 2)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32)))"#;
    assert!(new_inst(missing_export).is_err());

    let oob_ptr = (999_999_u64 << 32) | 16_u64;
    let oob_wat = format!(
        r#"(module
        (memory (export "memory") 1 2)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const {oob_ptr}))"#
    );
    let mut inst = new_inst(&oob_wat).unwrap();
    assert!(inst.on_event(b"{}").is_err());
    assert!(matches!(inst.on_event(b"{}"), Err(ErrorCode::BadState)));

    let huge_len = (MESSAGE_LIMIT as u64) + 1;
    let huge_packed = (1024_u64 << 32) | huge_len;
    let huge_wat = format!(
        r#"(module
        (memory (export "memory") 4 8)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const {huge_packed}))"#
    );
    let mut inst = new_inst(&huge_wat).unwrap();
    assert!(inst.on_event(b"{}").is_err());
    assert!(matches!(inst.on_event(b"{}"), Err(ErrorCode::BadState)));
}

#[test]
fn wat_capability_denial_and_unprivileged_host_log() {
    let denied_fixture = WatFixture::new(
        "wat.cap.denied",
        r#"{"ok":[{"command":"show_toast","message":"unauthorized"}]}"#,
        vec![],
        false,
    );
    let mut denied_ctrl = denied_fixture.load();
    until(
        || {
            denied_ctrl.poll();
            matches!(denied_ctrl.status(), Status::Failed(ErrorCode::Denied))
        },
        Duration::from_secs(4),
    );
    denied_ctrl.stop();
    until(|| denied_ctrl.is_quiescent(), Duration::from_secs(4));

    let log_fixture = WatFixture::new(
        "wat.cap.log",
        r#"{"ok":[{"command":"log","level":"warn","message":"init ok"}]}"#,
        vec![],
        false,
    );
    let mut log_ctrl = log_fixture.load();
    until(
        || {
            let _ = log_ctrl.poll();
            log_ctrl.status() == Status::Ready
        },
        Duration::from_secs(4),
    );
    log_ctrl.stop();
    until(|| log_ctrl.is_quiescent(), Duration::from_secs(4));
}

#[test]
fn wat_pixel_grid_and_view_timer_validation_boundaries() {
    use iris_api::{PluginModal, ViewTimer, Widget};

    let encode_modal = |modal: PluginModal| {
        serde_json::to_string(&serde_json::json!({
            "ok": [HostCommand::OpenModal { modal: Box::new(modal) }]
        }))
        .unwrap()
    };

    let valid_modal = encode_modal(PluginModal {
        id: "qr_login".into(),
        title: "Login".into(),
        subtitle: None,
        submit_label: None,
        cancel_label: None,
        timer: Some(ViewTimer {
            action_id: "poll_qr".into(),
            interval_ms: 2_000,
        }),
        widgets: vec![Widget::PixelGrid {
            label: "QR".into(),
            rows: vec!["101".into(), "010".into(), "101".into()],
            caption: Some("Scan".into()),
            copy_text: None,
        }],
    });

    let ok_fixture = WatFixture::new(
        "wat.ui.valid",
        &valid_modal,
        vec![Capability::UiModal],
        true,
    );
    let mut ok_ctrl = ok_fixture.load();
    until(
        || {
            let _ = ok_ctrl.poll();
            ok_ctrl.status() == Status::Ready
        },
        Duration::from_secs(4),
    );
    let win = Control::new_window_id().unwrap();
    ok_fixture.open_window(&mut ok_ctrl, win);
    ok_ctrl
        .send(
            win,
            HostEvent::Command {
                id: "open".into(),
                snapshot: None,
            },
        )
        .unwrap();
    let mut received = None;
    until(
        || {
            received = ok_ctrl.poll();
            received.is_some()
        },
        Duration::from_secs(4),
    );
    assert!(matches!(
        received.unwrap().commands.as_slice(),
        [HostCommand::OpenModal { .. }]
    ));
    ok_ctrl.stop();
    until(|| ok_ctrl.is_quiescent(), Duration::from_secs(4));

    let invalid_cases = [
        encode_modal(PluginModal {
            id: "bad_fast".into(),
            title: "Bad".into(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            timer: Some(ViewTimer {
                action_id: "poll".into(),
                interval_ms: 500,
            }),
            widgets: vec![],
        }),
        encode_modal(PluginModal {
            id: "bad_slow".into(),
            title: "Bad".into(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            timer: Some(ViewTimer {
                action_id: "poll".into(),
                interval_ms: 300_001,
            }),
            widgets: vec![],
        }),
        encode_modal(PluginModal {
            id: "bad_jagged".into(),
            title: "Bad".into(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            timer: None,
            widgets: vec![Widget::PixelGrid {
                label: String::new(),
                rows: vec!["101".into(), "10".into()],
                caption: None,
                copy_text: None,
            }],
        }),
        encode_modal(PluginModal {
            id: "bad_char".into(),
            title: "Bad".into(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            timer: None,
            widgets: vec![Widget::PixelGrid {
                label: String::new(),
                rows: vec!["1X".into()],
                caption: None,
                copy_text: None,
            }],
        }),
        encode_modal(PluginModal {
            id: "bad_wide".into(),
            title: "Bad".into(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            timer: None,
            widgets: vec![Widget::PixelGrid {
                label: String::new(),
                rows: vec!["1".repeat(178)],
                caption: None,
                copy_text: None,
            }],
        }),
    ];

    for (idx, payload) in invalid_cases.iter().enumerate() {
        let fixture = WatFixture::new(
            &format!("wat.ui.invalid{idx}"),
            payload,
            vec![Capability::UiModal],
            true,
        );
        let mut ctrl = fixture.load();
        let win = Control::new_window_id().unwrap();
        until(
            || {
                let _ = ctrl.poll();
                matches!(
                    ctrl.status(),
                    Status::Ready | Status::Failed(ErrorCode::InvalidArgument)
                )
            },
            Duration::from_secs(4),
        );
        if ctrl.status() == Status::Ready {
            fixture.open_window(&mut ctrl, win);
            if let Err(err) = ctrl.send(
                win,
                HostEvent::Command {
                    id: "trigger".into(),
                    snapshot: None,
                },
            ) {
                assert_eq!(err, ErrorCode::Cancelled);
            }
            until(
                || {
                    let _ = ctrl.poll();
                    matches!(ctrl.status(), Status::Failed(ErrorCode::InvalidArgument))
                },
                Duration::from_secs(4),
            );
        }
        assert_eq!(ctrl.status(), Status::Failed(ErrorCode::InvalidArgument));
        ctrl.stop();
        until(|| ctrl.is_quiescent(), Duration::from_secs(4));
    }
}

struct CaptureSink {
    plugin_id: String,
    events: Mutex<Vec<TelemetryEvent>>,
}

impl TelemetrySink for CaptureSink {
    fn record(&self, event: &TelemetryEvent) {
        if event.category == "plugin"
            && event.event == "log"
            && event
                .fields
                .get("plugin")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p == self.plugin_id)
        {
            self.events.lock().unwrap().push(event.clone());
        }
    }
}

#[test]
fn wat_host_log_redacts_secrets_and_enforces_rate_limit() {
    let plugin_id = format!("wat.log.rate.p{}", std::process::id());
    let sink = Arc::new(CaptureSink {
        plugin_id: plugin_id.clone(),
        events: Mutex::new(Vec::new()),
    });
    logging::register_sink(sink.clone());

    let secret_payload = serde_json::json!({
        "ok": [{
            "command": "log",
            "level": "info",
            "message": "auth Bearer secret-tok-999 https://alice:hunter2@nas.local/a?token=xyz999"
        }]
    })
    .to_string();
    let fixture = WatFixture::new(&plugin_id, &secret_payload, vec![], false);
    let mut ctrl = fixture.load();
    until(
        || {
            let _ = ctrl.poll();
            ctrl.status() == Status::Ready
        },
        Duration::from_secs(4),
    );

    let win = Control::new_window_id().unwrap();
    fixture.open_window(&mut ctrl, win);

    for _ in 0..28 {
        ctrl.send(
            win,
            HostEvent::Command {
                id: "burst".into(),
                snapshot: None,
            },
        )
        .unwrap();
        let mut got = false;
        until(
            || {
                if ctrl.poll().is_some() {
                    got = true;
                }
                got
            },
            Duration::from_secs(4),
        );
    }

    let captured = sink.events.lock().unwrap().clone();
    assert!(
        !captured.is_empty() && captured.len() <= 20,
        "expected 1..=20 rate-limited plugin log events, got {}",
        captured.len()
    );
    for ev in &captured {
        let msg = ev.fields["message"].as_str().unwrap_or("");
        assert!(!msg.contains("secret-tok-999"), "Bearer leaked: {msg}");
        assert!(!msg.contains("hunter2"), "userinfo password leaked: {msg}");
        assert!(!msg.contains("xyz999"), "query token leaked: {msg}");
    }

    ctrl.stop();
    until(|| ctrl.is_quiescent(), Duration::from_secs(4));
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
fn wat_stream_role_abi_replies_and_fails_cleanly() {
    let empty = r#"{"ok":[]}"#;
    let key_needle = r#""context_blob":""#;
    let req_needle = r#""request_id":""#;
    let fail_needle = r#""resource_id":"fail""#;
    let hang_needle = r#""resource_id":"hang""#;
    let esc = |s: &str| -> String { s.as_bytes().iter().map(|b| format!("\\{b:02x}")).collect() };
    let empty_packed = (1024_u64 << 32) | empty.len() as u64;

    let wasm = wat::parse_str(format!(
        r#"(module
        (import "iris_v2" "host_blob_new" (func $blob_new (param i32 i32) (result i64)))
        (import "iris_v2" "host_blob_len" (func $blob_len (param i64) (result i64)))
        (import "iris_v2" "host_blob_read" (func $blob_read (param i64 i64 i32 i32) (result i64)))
        (import "iris_v2" "host_stream_reply" (func $stream_reply (param i64 i64 i32) (result i64)))
        (import "iris_v2" "host_stream_fail" (func $stream_fail (param i64 i32) (result i32)))
        (memory (export "memory") 4 8)
        (global $key (mut i32) (i32.const 0))
        (data (i32.const 1024) "{empty_esc}")
        (data (i32.const 1100) "{key_esc}")
        (data (i32.const 1132) "{req_esc}")
        (data (i32.const 1164) "{fail_esc}")
        (data (i32.const 1196) "{hang_esc}")
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
            (local $blob i64)
            (local $i i32)
            (local.set $end (i32.add (local.get $ptr) (local.get $len)))
            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1100) (i32.const {key_len})))
            (if (i32.ge_s (local.get $pos) (i32.const 0))
                (then
                    (local.set $id (call $parse_u64 (local.get $pos) (local.get $end)))
                    (if (i64.eq (call $blob_len (local.get $id)) (i64.const 1))
                        (then
                            (drop (call $blob_read (local.get $id) (i64.const 0) (i32.const 512) (i32.const 1)))
                            (global.set $key (i32.load8_u (i32.const 512)))))
                    (return (i64.const {empty_packed}))))
            (local.set $pos (call $find (local.get $ptr) (local.get $len) (i32.const 1132) (i32.const {req_len})))
            (if (i32.ge_s (local.get $pos) (i32.const 0))
                (then
                    (local.set $id (call $parse_u64 (local.get $pos) (local.get $end)))
                    (if (i32.ge_s (call $find (local.get $ptr) (local.get $len) (i32.const 1196) (i32.const {hang_len})) (i32.const 0))
                        (then (return (i64.const {empty_packed}))))
                    (if (i32.ge_s (call $find (local.get $ptr) (local.get $len) (i32.const 1164) (i32.const {fail_len})) (i32.const 0))
                        (then
                            (drop (call $stream_fail (local.get $id) (i32.const -9))))
                        (else
                            (local.set $i (i32.const 0))
                            (block $fill_done
                                (loop $fill
                                    (br_if $fill_done (i32.ge_u (local.get $i) (i32.const 8)))
                                    (i32.store8
                                        (i32.add (i32.const 4096) (local.get $i))
                                        (i32.xor (i32.add (i32.const 64) (local.get $i)) (global.get $key)))
                                    (local.set $i (i32.add (local.get $i) (i32.const 1)))
                                    (br $fill)))
                            (local.set $blob (call $blob_new (i32.const 4096) (i32.const 8)))
                            (drop (call $stream_reply (local.get $id) (local.get $blob) (i32.const 1)))))))
            (i64.const {empty_packed})))"#,
        empty_esc = esc(empty),
        key_esc = esc(key_needle),
        req_esc = esc(req_needle),
        fail_esc = esc(fail_needle),
        hang_esc = esc(hang_needle),
        key_len = key_needle.len(),
        req_len = req_needle.len(),
        fail_len = fail_needle.len(),
        hang_len = hang_needle.len(),
    ))
    .unwrap();

    struct NoStreamIo;
    impl Executor for NoStreamIo {
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

    let engine = RuntimeEngine::new().unwrap();
    let module = engine.compile(&wasm, &CancelToken::new()).unwrap();
    let ledger = Ledger::default();
    let context = StreamContext {
        version: Id(1),
        blob: Id(1),
        origin_grants: Vec::new(),
        transports: Vec::new(),
        expires_unix_ms: None,
    };
    let store = Arc::new(
        StreamStore::spawn(
            Spawn {
                engine,
                module,
                role: Role::Stream,
                owner: "wat-stream".into(),
                grants: BTreeSet::new(),
                ledger: ledger.clone(),
                io: IoPool::new().unwrap(),
                executor: Arc::new(NoStreamIo),
                generation: Generation::default(),
                wake: Arc::new(|| {}),
            },
            new_session_id().unwrap(),
            &context,
            Arc::new(Blob::new(&ledger, "wat-stream", vec![0x5a]).unwrap()),
        )
        .unwrap(),
    );

    let cancel = HttpCancel::default();
    let reader = store.reader("good".into(), Some(8)).unwrap();
    let reply = reader.read_at(0, 8, &cancel).unwrap();
    assert!(reply.eof);
    assert_eq!(
        reply.blob.bytes,
        (0u8..8).map(|i| (64 + i) ^ 0x5a).collect::<Vec<_>>()
    );
    drop(reply);

    let updated = StreamContext {
        version: Id(2),
        ..context
    };
    store
        .refresh(
            Id(1),
            &updated,
            Arc::new(Blob::new(&ledger, "wat-stream", vec![0x33]).unwrap()),
        )
        .unwrap();
    let refreshed = reader.read_at(0, 8, &cancel).unwrap();
    assert_eq!(
        refreshed.blob.bytes,
        (0u8..8).map(|i| (64 + i) ^ 0x33).collect::<Vec<_>>()
    );
    drop(refreshed);

    let failing = store.reader("fail".into(), Some(8)).unwrap();
    assert_eq!(
        failing.read_at(0, 8, &cancel).err(),
        Some(ErrorCode::IoError)
    );

    let hanging = store.reader("hang".into(), Some(8)).unwrap();
    let hang_cancel = HttpCancel::default();
    let worker_cancel = hang_cancel.clone();
    let hang_thread = std::thread::spawn(move || hanging.read_at(0, 8, &worker_cancel).err());
    until(
        || ledger.used(None, Resource::StreamRead) == 1,
        Duration::from_secs(2),
    );
    let t0 = Instant::now();
    hang_cancel.cancel();
    assert_eq!(hang_thread.join().unwrap(), Some(ErrorCode::Cancelled));
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "GuestReader cancel took {:?}",
        t0.elapsed()
    );

    store.stop();
    until(|| store.is_quiescent(), Duration::from_secs(4));
    drop((reader, failing, store));
    assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
    assert_eq!(ledger.used(None, Resource::StreamRead), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
