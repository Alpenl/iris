use super::*;

#[test]
fn non_quarantine_control_failure_fails_in_flight_requests_and_clears_readiness() {
    let mut f = Fixture::new();
    let init_reply = r#"{"ok":[{"command":"set_sources","sources":[{"id":"src","name":"Src","subtitle":null,"icon":null,"badge":null,"auth_command_id":null}]}]}"#;
    let err_reply = r#"{"error":{"code":-6,"message":"Timeout"}}"#;
    let init_esc = init_reply
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let err_esc = err_reply
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let wasm = wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 4 8)
        (data (i32.const 2048) "{init_esc}")
        (data (i32.const 4096) "{err_esc}")
        (data (i32.const 6144) "\7b\22ok\22:[]\7d")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        (func $has4 (param $ptr i32) (param $len i32) (param $needle i32) (result i32)
            (local $i i32)
            (block $end (loop $scan
                local.get $i i32.const 4 i32.add local.get $len i32.gt_u br_if $end
                local.get $ptr local.get $i i32.add i32.load align=1 local.get $needle i32.eq
                if i32.const 1 return end
                local.get $i i32.const 1 i32.add local.set $i br $scan))
            i32.const 0)
        (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
            local.get $ptr local.get $len i32.const {init_tag} call $has4
            if (result i64) i64.const {init_packed} else
                local.get $ptr local.get $len i32.const {browse_tag} call $has4
                if (result i64) i64.const {err_packed} else i64.const {ok_packed} end
            end))"#,
        init_tag = i32::from_le_bytes(*b"init"),
        browse_tag = i32::from_le_bytes(*b"brow"),
        init_packed = (2048_u64 << 32) | init_reply.len() as u64,
        err_packed = (4096_u64 << 32) | err_reply.len() as u64,
        ok_packed = (6144_u64 << 32) | 9,
    ))
    .unwrap();
    let manifest = serde_json::json!({
        "id": "test.timeout", "name": "Timeout source", "version": "1",
        "protocol": 2, "abi": 2, "entry": "plugin.wasm", "permissions": ["media_source"]
    });
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("plugin.json", serde_json::to_vec(&manifest).unwrap()),
        ("plugin.wasm", wasm),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&bytes).unwrap();
    }
    f.install_bytes(&archive.finish().unwrap().into_inner());

    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| {
        client.is_ready("test.timeout")
            && client
                .catalog()
                .plugins
                .iter()
                .any(|p| p.id == "test.timeout" && p.sources.is_some())
    });
    assert!(client.access("test.timeout").is_ok());

    let (_view, req_id) = client
        .browse(
            "test.timeout",
            crate::requests::BrowseTarget {
                source: "src".into(),
                folder: None,
                query: None,
                filters: Default::default(),
            },
        )
        .unwrap();

    let mut saw_request_error = false;
    wait(|| {
        for notice in client.poll() {
            if let Notice::Error {
                plugin: Some(p),
                request: Some(r),
                code: ErrorCode::Timeout,
                ..
            } = notice
                && p == "test.timeout"
                && r == req_id
            {
                saw_request_error = true;
            }
        }
        saw_request_error
            && client.catalog().plugins.iter().any(|p| {
                p.id == "test.timeout" && p.status == PluginStatus::Failed(ErrorCode::Timeout)
            })
    });

    // Binding stays live for independent playback, while dead control is removed from readiness/access.
    let info = client
        .catalog()
        .plugins
        .iter()
        .find(|p| p.id == "test.timeout")
        .cloned()
        .unwrap();
    assert!(info.binding.as_ref().unwrap().is_live());
    assert!(!client.is_ready("test.timeout"));
    assert_eq!(
        client.access("test.timeout").err(),
        Some(ErrorCode::StaleHandle)
    );
    assert_eq!(
        client
            .browse(
                "test.timeout",
                crate::requests::BrowseTarget {
                    source: "src".into(),
                    folder: None,
                    query: None,
                    filters: Default::default(),
                },
            )
            .err(),
        Some(ErrorCode::StaleHandle)
    );

    drop((info, client, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn plugin_manager_poll_stays_clean_when_zero_plugins_are_ready() {
    use crate::ui::WindowSession;

    // 1. Unavailable client: first poll drains the one-shot error, subsequent polls are clean.
    let (unavailable, _) = Client::unavailable(ErrorCode::Internal);
    let mut offline_ui = WindowSession::new(unavailable, None);
    let first = offline_ui.poll();
    assert_eq!(first.notices.len(), 1);
    assert!(!first.status_changed);
    assert!(!first.dirty.sources);
    let second = offline_ui.poll();
    assert!(!second.status_changed);
    assert!(!second.dirty.sources);
    assert!(!second.should_notify_ui(false, true, true));

    // 2. Hub with all plugins disabled (`slots` remains empty after catalog sync).
    let mut f = Fixture::new();
    f.install("test.disabled");
    let hub = Hub::new(Config {
        bootstrap: None,
        installer: f.installer.clone(),
        runtime: f.setup.clone(),
        disabled: BTreeSet::from(["test.disabled".into()]),
    })
    .unwrap();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut ui = WindowSession::new(client, None);
    wait(|| {
        ui.poll();
        ui.list()
            .iter()
            .any(|p| p.id == "test.disabled" && p.status == PluginStatus::Disabled)
    });
    let idle = ui.poll();
    assert!(!idle.status_changed);
    assert!(!idle.dirty.sources);
    assert!(!idle.should_notify_ui(false, true, true));

    drop((ui, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}
