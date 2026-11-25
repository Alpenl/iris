//! Default-enabled pure-WAT ABI v2 contract tests (no external Zig/wasm32 toolchain needed).

use super::*;
use iris_api::v2::{HostEvent, MESSAGE_LIMIT, NetworkDeclaration};
use iris_core::{
    logging::{self, TelemetryEvent, TelemetrySink},
    plugin_v2::{
        installation::{Approval, Installed, Installer},
        package::{Package, Provenance},
        runtime::{Control, Resources, Setup},
    },
    vault::CredentialVault,
};
use std::{
    io::{Cursor, Write},
    path::PathBuf,
    sync::Mutex,
};
use zip::{ZipWriter, write::SimpleFileOptions};

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

    fn load(&self) -> Control {
        Control::load(
            &self.installer,
            self.installed.clone(),
            self.setup.clone(),
            &AtomicBool::new(false),
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
    let cancel = Arc::new(AtomicBool::new(false));
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
            services: Box::new(NoServices),
        })
    };

    // Wrong ABI version must be rejected during handshake.
    let bad_abi = r#"(module
        (memory (export "memory") 1 2)
        (func (export "iris_abi_version") (result i32) i32.const 1)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#;
    assert!(new_inst(bad_abi).is_err());

    // Missing required export must be rejected during handshake.
    let missing_export = r#"(module
        (memory (export "memory") 1 2)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 64)
        (func (export "iris_dealloc") (param i32 i32)))"#;
    assert!(new_inst(missing_export).is_err());

    // Out-of-bounds return pointer must fail and poison the instance.
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

    // Payload exceeding MESSAGE_LIMIT must fail and poison the instance.
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
    // 1. ShowToast without Notifications grant must be denied and quarantine the control actor.
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

    // 2. HostCommand::Log requires no capability and is permitted as a global Init command.
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
    ok_ctrl
        .open_window(
            win,
            Generation::default(),
            None,
            Arc::new(|_, _, _| Err(ErrorCode::Denied)),
        )
        .unwrap();
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

    // Invalid ViewTimer (< 1000ms, > 300_000ms) and malformed PixelGrid must fail with InvalidArgument.
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
        // Send on a window if Init did not already trigger failure, or let Init fail directly.
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
            ctrl.open_window(
                win,
                Generation::default(),
                None,
                Arc::new(|_, _, _| Err(ErrorCode::Denied)),
            )
            .unwrap();
            ctrl.send(
                win,
                HostEvent::Command {
                    id: "trigger".into(),
                    snapshot: None,
                },
            )
            .unwrap();
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
    ctrl.open_window(
        win,
        Generation::default(),
        None,
        Arc::new(|_, _, _| Err(ErrorCode::Denied)),
    )
    .unwrap();

    // Burst 28 rapid commands inside one 1-second rate-limit window (Init already used 1 slot).
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
