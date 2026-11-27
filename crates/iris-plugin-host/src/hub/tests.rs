use super::*;
use crate::{
    installation::Approval,
    package::{Package, Provenance},
    runtime::Resources,
};
use iris_core::vault::CredentialVault;
use std::{
    io::{Cursor, Write},
    time::{Duration, Instant},
};

fn package(id: &str) -> Vec<u8> {
    let reply = r#"{"ok":[{"command":"show_toast","message":"window"}]}"#;
    package_with(id, &[Capability::Notifications], reply, false)
}
fn package_with(id: &str, permissions: &[Capability], reply: &str, once: bool) -> Vec<u8> {
    let data = reply
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let wasm = wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 4 8)
        (global $seen (mut i32) (i32.const 0))
        (data (i32.const 2048) "{data}")
        (data (i32.const 4096) "\7b\22ok\22:[]\7d")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        (func $window (param $ptr i32) (param $len i32) (result i32)
            (local $i i32)
            (block $end (loop $scan
                local.get $i i32.const 16 i32.add local.get $len i32.gt_u br_if $end
                local.get $ptr local.get $i i32.add i64.load align=1 i64.const {first} i64.eq
                local.get $ptr local.get $i i32.add i64.load offset=8 align=1 i64.const {second} i64.eq
                i32.and if i32.const 1 return end
                local.get $i i32.const 1 i32.add local.set $i br $scan))
            i32.const 0)
        (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
            local.get $ptr local.get $len call $window
            global.get $seen i32.eqz i32.const {} i32.or i32.and
            if (result i64) i32.const 1 global.set $seen i64.const {} else i64.const {} end))"#,
        u8::from(!once),
        (2048_u64 << 32) | reply.len() as u64,
        (4096_u64 << 32) | 9,
        first = u64::from_le_bytes(*b"\"event\":"),
        second = u64::from_le_bytes(*b"\"window\"")
    ))
    .unwrap();
    let manifest = serde_json::json!({"id":id,"name":"Hub fixture","version":"1","protocol":2,
        "abi":2,"entry":"plugin.wasm","permissions":permissions});
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
    archive.finish().unwrap().into_inner()
}
struct Fixture {
    root: PathBuf,
    resources: Resources,
    installer: Installer,
    setup: Setup,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-v2-hub-{}-{}",
            std::process::id(),
            super::super::instance::next_id().unwrap()
        ));
        let resources = Resources::new().unwrap();
        let installer = Installer::new(root.join("packages"), resources.ledger.clone());
        let setup = Setup {
            resources: resources.clone(),
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        };
        Self {
            root,
            resources,
            installer,
            setup,
        }
    }
    fn install(&mut self, id: &str) -> Installed {
        let bytes = package(id);
        self.install_bytes(&bytes)
    }
    fn install_bytes(&mut self, bytes: &[u8]) -> Installed {
        let package = Package::from_zip(bytes, Provenance::User).unwrap();
        self.installer
            .install(
                bytes,
                Approval {
                    digest: package.digest,
                    grants: package.manifest.permissions.into_iter().collect(),
                    network: Default::default(),
                    previous: None,
                },
            )
            .unwrap()
    }
    fn hub(&self) -> Hub {
        Hub::new(Config {
            bootstrap: None,
            installer: self.installer.clone(),
            runtime: self.setup.clone(),
            disabled: BTreeSet::new(),
        })
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[track_caller]
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !predicate() {
        assert!(Instant::now() < deadline, "coordinator deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn toast(client: &Client) -> bool {
    client.poll().iter().any(|notice| matches!(notice, Notice::Commands(batch)
        if batch.indices.iter().any(|i| matches!(batch.update.commands[*i], iris_api::v2::HostCommand::ShowToast { .. }))))
}
fn ping(client: &Client, id: &str) {
    client
        .send(
            id,
            HostEvent::Command {
                id: "ping".into(),
                snapshot: None,
            },
        )
        .unwrap();
}
fn ready(client: &Client, id: &str) -> bool {
    client
        .catalog()
        .plugins
        .iter()
        .any(|p| p.id == id && p.status == PluginStatus::Ready)
}
fn client_inbox(client: &Client) -> &Arc<Inbox> {
    match &client.state {
        client::ClientState::Connected { inbox, .. } => inbox,
        client::ClientState::Unavailable { .. } => panic!("expected connected client"),
    }
}

#[test]
fn wasm_fixture_recognizes_both_json_member_orders() {
    use crate::engine::RuntimeEngine;
    use crate::instance::{Instance, InstanceConfig};
    let package = Package::from_zip(&package("test.order"), Provenance::User).unwrap();
    let engine = RuntimeEngine::new().unwrap();
    let cancel = iris_net::concurrency::CancelToken::new();
    let module = engine.compile(package.wasm().unwrap(), &cancel).unwrap();
    let mut guest = Instance::new(InstanceConfig {
        engine,
        module,
        role: iris_api::v2::Role::Control,
        owner: "test-order".into(),
        grants: BTreeSet::from([Capability::Notifications]),
        ledger: Default::default(),
        cancel,
    })
    .unwrap();
    for input in [br#"{"event":"window","window":"1","generation":{"media":"0","seek":"0","context":"0"},"body":{"event":"window_opened","snapshot":null}}"#.as_slice(),
                  br#"{"body":{"event":"window_opened","snapshot":null},"event":"window","generation":{"context":"0","media":"0","seek":"0"},"window":"1"}"#.as_slice()] {
        assert!(matches!(guest.on_event(input).unwrap(), iris_api::v2::Reply::Ok { ok } if ok.len() == 1));
    }
}

#[test]
fn single_window_enforces_exclusivity_and_allows_clean_reopen() {
    let mut f = Fixture::new();
    let record = f.install("test.windows");
    let hub = f.hub();
    let (first, _) = hub.open_window(Generation::default(), None).unwrap();
    assert!(matches!(
        hub.open_window(Generation::default(), None),
        Err(ErrorCode::Backpressure)
    ));
    wait(|| toast(&first));
    assert!(ready(&first, "test.windows"));
    assert_eq!(
        f.resources
            .ledger
            .used(Some(&record.installation_id), Resource::ControlStore),
        1
    );
    ping(&first, "test.windows");
    wait(|| toast(&first));
    first.shutdown();
    let (second, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| toast(&second));
    ping(&second, "test.windows");
    wait(|| toast(&second));
    let binding = second.catalog().plugins[0].binding.clone().unwrap();
    second.set_enabled("test.windows", false).unwrap();
    wait(|| !binding.is_live());
    wait(|| f.resources.ledger.used(None, Resource::ControlStore) == 0);
    assert!(
        second
            .catalog()
            .plugins
            .iter()
            .any(|p| p.status == PluginStatus::Disabled)
    );
    drop((first, second, binding, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn saturated_window_revokes_the_offender_and_keeps_failure_visible() {
    let mut f = Fixture::new();
    f.install("test.flood");
    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| toast(&client));
    let binding = client.catalog().plugins[0].binding.clone().unwrap();
    for _ in 0..40 {
        ping(&client, "test.flood");
        std::thread::sleep(Duration::from_millis(5));
    }
    wait(|| !binding.is_live());
    wait(|| f.resources.ledger.used(None, Resource::ControlStore) == 0);
    assert!(
        client
            .catalog()
            .plugins
            .iter()
            .any(|p| p.status == PluginStatus::Failed(ErrorCode::Backpressure))
    );
    assert!(!toast(&client), "revoked queued batches cannot apply later");
    drop((client, binding, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn review_approval_is_window_bound_and_installs_the_reviewed_bytes() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.root).unwrap();
    let path = f.root.join("review.irisplugin");
    let bytes = package("test.review");
    std::fs::write(&path, &bytes).unwrap();
    let hub = f.hub();
    let (first, _) = hub.open_window(Generation::default(), None).unwrap();
    let stale_id = first.review(path.clone()).unwrap();
    wait(|| {
        first
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::Review(r) if r.id == stale_id))
    });
    assert_eq!(
        f.resources.ledger.used(None, Resource::ControlStore),
        0,
        "review must not execute a module"
    );
    first.shutdown();
    let (second, _) = hub.open_window(Generation::default(), None).unwrap();
    second
        .approve(stale_id, BTreeSet::from([Capability::Notifications]), false)
        .unwrap();
    wait(|| {
        second.poll().iter().any(|n| {
            matches!(
                n,
                Notice::Error {
                    code: ErrorCode::StaleHandle,
                    request: Some(r),
                    ..
                } if *r == stale_id
            )
        })
    });
    assert!(f.installer.record("test.review").unwrap().is_none());

    let id = second.review(path.clone()).unwrap();
    let mut summary = None;
    wait(|| {
        for notice in second.poll() {
            if let Notice::Review(review) = notice {
                summary = Some(review);
            }
        }
        summary.is_some()
    });
    std::fs::write(&path, b"file changed after review").unwrap();
    second
        .approve(id, BTreeSet::from([Capability::Notifications]), false)
        .unwrap();
    wait(|| f.installer.record("test.review").unwrap().is_some());
    assert_eq!(
        f.installer.record("test.review").unwrap().unwrap().digest,
        summary.as_ref().unwrap().digest
    );
    wait(|| toast(&second));
    drop((first, second, summary, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn stale_batches_cannot_cross_media_generations() {
    let mut f = Fixture::new();
    f.install("test.generation");
    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| toast(&client));
    ping(&client, "test.generation");
    wait(|| !client_inbox(&client).lock().notices.is_empty());
    client
        .advance(Generation {
            media: Id(1),
            seek: Id(0),
            context: Id(0),
        })
        .unwrap();
    assert!(!toast(&client));
    ping(&client, "test.generation");
    wait(|| toast(&client));
    drop((client, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn origin_consent_cannot_be_approved_by_reopened_window_or_after_close() {
    let mut f = Fixture::new();
    let bytes = package_with(
        "test.origin",
        &[Capability::UiModal, Capability::NetHttp],
        r#"{"ok":[{"command":"request_origin","request_id":"42","origin":"http://192.168.1.50:5244/dav"}]}"#,
        true,
    );
    let record = f.install_bytes(&bytes);
    let hub = f.hub();
    let (first, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut review = None;
    wait(|| {
        for notice in first.poll() {
            if let Notice::Origin(value) = notice {
                review = Some(value);
            }
        }
        review.is_some()
    });
    let review = review.unwrap();
    assert_eq!(review.origin, "http://192.168.1.50:5244");
    assert!(review.private && review.plaintext);
    assert!(matches!(
        hub.open_window(Generation::default(), None),
        Err(ErrorCode::Backpressure)
    ));
    let path = f
        .setup
        .state_directory
        .join("origins")
        .join(format!("{}.json", record.installation_id));
    assert!(!path.exists());
    first.shutdown();
    assert!(matches!(
        first.confirm_origin(review.id, true, false),
        Err(ErrorCode::Cancelled)
    ));
    let (second, _) = hub.open_window(Generation::default(), None).unwrap();
    second.confirm_origin(review.id, true, false).unwrap();
    wait(|| {
        second.poll().iter().any(|n| {
            matches!(
                n,
                Notice::Error {
                    code: ErrorCode::StaleHandle,
                    ..
                }
            )
        })
    });
    assert!(!path.exists());
    drop((review, first, second, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn presentation_slots_keep_accounting_and_only_dirty_changed_values() {
    use crate::ui::PluginManager;
    let mut f = Fixture::new();
    f.install_bytes(&package_with(
        "test.presentation",
        &[Capability::UiSidebar],
        r#"{"ok":[{"command":"set_sidebar","panel":{"title":"Panel","sections":[]}}]}"#,
        false,
    ));
    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut ui = PluginManager::new(client, None);
    wait(|| {
        ui.poll();
        !ui.sidebar_panels().is_empty()
    });
    assert_eq!(ui.sidebar_panels()[0].1.title, "Panel");
    ui.poll();
    let report = ui.send_to(
        "test.presentation",
        HostEvent::Command {
            id: "same".into(),
            snapshot: None,
        },
    );
    assert!(report.notices.is_empty());
    // Wait for the callback output, then verify the value-equality redraw gate.
    wait(|| !client_inbox(ui.client()).lock().notices.is_empty());
    assert!(!ui.poll().dirty.sidebar);
    let report =
        ui.set_enabled_with_snapshot("test.presentation", false, PlaybackSnapshot::default());
    assert!(report.notices.is_empty());
    wait(|| {
        ui.poll();
        ui.sidebar_panels().is_empty()
    });
    drop((ui, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn two_isolated_plugin_hosts_do_not_share_state_and_inbox_snapshot_is_consistent() {
    use crate::product::PluginHost;
    let mut f1 = Fixture::new();
    let mut f2 = Fixture::new();
    f1.install("test.host_a");
    f2.install("test.host_b");
    let host1 = PluginHost::from_parts(f1.resources.clone(), f1.hub());
    let host2 = PluginHost::from_parts(f2.resources.clone(), f2.hub());
    let (mut m1, _) = host1.open_manager(&[], "dark");
    let (mut m2, _) = host2.open_manager(&[], "dark");
    wait(|| {
        m1.poll();
        m2.poll();
        m1.client().is_ready("test.host_a") && m2.client().is_ready("test.host_b")
    });
    assert!(!m1.client().is_ready("test.host_b"));
    assert!(!m2.client().is_ready("test.host_a"));
    m1.client()
        .advance(Generation {
            media: Id(7),
            seek: Id(3),
            context: Id(1),
        })
        .unwrap();
    let snap = client_inbox(m1.client()).snapshot();
    assert_eq!(snap.generation.media, Id(7));
    assert!(!snap.closed);
    drop(snap);
    host1.shutdown();
    drop((m1, host1));
    wait(|| f1.resources.ledger.used(None, Resource::HostBytes) == 0);
    assert!(m2.client().is_ready("test.host_b"));
    host2.shutdown();
    drop((m2, host2));
    wait(|| f2.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn media_lifecycle_coalesces_past_a_full_ordinary_queue() {
    // No coordinator thread: deterministically hold a fully occupied mailbox.
    let ledger = crate::ledger::Ledger::default();
    let lease = ledger
        .reserve("test-mailbox", &[(Resource::HostBytes, 1 << 20)])
        .unwrap();
    let shared = Arc::new(Shared {
        gate: Mutex::new(Gate::default()),
        changed: Condvar::new(),
        catalog: Mutex::new(Arc::new(Catalog {
            revision: 0,
            plugins: Vec::new(),
        })),
        ledger,
        _lease: lease,
    });
    let hub = Hub(Arc::new(Connection(shared.clone())));
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    shared.gate.lock().unwrap().commands.clear();
    for _ in 0..64 {
        client.update_snapshot(PlaybackSnapshot::default()).unwrap();
    }
    assert_eq!(
        client.update_snapshot(PlaybackSnapshot::default()),
        Err(ErrorCode::Backpressure)
    );
    for seek in 1..100 {
        client
            .advance(Generation {
                seek: Id(seek),
                ..Generation::default()
            })
            .unwrap();
        assert_eq!(shared.gate.lock().unwrap().commands.len(), 65);
    }
    assert_eq!(client.generation().seek, Id(99));
    assert_eq!(
        client.advance(Generation::default()),
        Err(ErrorCode::StaleHandle)
    );
    client.shutdown();
    assert_eq!(
        client.advance(Generation {
            media: Id(1),
            ..Generation::default()
        }),
        Err(ErrorCode::Cancelled)
    );
}

#[test]
fn uninstall_supports_full_wipe_and_optional_credential_retention_with_opt_in_restore() {
    let mut f = Fixture::new();
    let pkg = package("test.uninstall");
    let pkg_path = f.root.join("test.uninstall.irisplugin");
    std::fs::create_dir_all(&f.root).unwrap();
    std::fs::write(&pkg_path, &pkg).unwrap();
    let record = f.install_bytes(&pkg);
    let scope = format!("plugin-v2.{}", record.installation_id);
    f.setup
        .vault
        .set_secret(&scope, "bili.session", Some("sess=1"))
        .unwrap();
    let state_file = f
        .setup
        .state_directory
        .join(format!("{}.json", record.installation_id));
    let origin_file = f
        .setup
        .state_directory
        .join("origins")
        .join(format!("{}.json", record.installation_id));
    std::fs::create_dir_all(origin_file.parent().unwrap()).unwrap();
    std::fs::write(&state_file, b"{}").unwrap();
    std::fs::write(&origin_file, b"{}").unwrap();

    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| ready(&client, "test.uninstall"));
    let binding = client.catalog().plugins[0].binding.clone().unwrap();

    // Official iris.* plugins are rejected with Denied.
    client.uninstall("iris.guides", false).unwrap();
    wait(|| {
        client.poll().iter().any(|n| {
            matches!(
                n,
                Notice::Error {
                    plugin: Some(p),
                    code: ErrorCode::Denied,
                    ..
                } if p == "iris.guides"
            )
        })
    });

    // 1. Uninstall with keep_data=true keeps state, origins, and vault secrets archived.
    client.uninstall("test.uninstall", true).unwrap();
    wait(|| !binding.is_live());
    wait(|| {
        client
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::Uninstalled(p) if p == "test.uninstall"))
    });
    assert!(f.installer.record("test.uninstall").unwrap().is_none());
    assert!(state_file.exists() && origin_file.exists());
    assert_eq!(
        f.setup.vault.get_secret(&scope, "bili.session").as_deref(),
        Some("sess=1")
    );
    wait(|| f.resources.ledger.used(None, Resource::ControlStore) == 0);

    // 2. Reinstall review surfaces the archived identity; opt-in keep_identity=true restores it.
    let review_id = client.review(pkg_path.clone()).unwrap();
    let mut summary = None;
    wait(|| {
        for n in client.poll() {
            if let Notice::Review(r) = n {
                summary = Some(r);
            }
        }
        summary.is_some()
    });
    let summary = summary.unwrap();
    assert!(summary.previous.is_none());
    assert_eq!(
        summary
            .archived
            .as_ref()
            .map(|a| a.installation_id.as_str()),
        Some(record.installation_id.as_str())
    );
    client
        .approve(review_id, BTreeSet::from([Capability::Notifications]), true)
        .unwrap();
    wait(|| ready(&client, "test.uninstall"));
    let restored = f.installer.record("test.uninstall").unwrap().unwrap();
    assert_eq!(restored.installation_id, record.installation_id);
    assert_eq!(
        f.setup.vault.get_secret(&scope, "bili.session").as_deref(),
        Some("sess=1")
    );

    // 3. Uninstall with keep_data=true, then reinstall with keep_identity=false:
    //    must issue a fresh identity AND scrub the declined archived state/vault secrets.
    client.uninstall("test.uninstall", true).unwrap();
    wait(|| {
        client
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::Uninstalled(p) if p == "test.uninstall"))
    });
    let review_id2 = client.review(pkg_path.clone()).unwrap();
    let mut summary2 = None;
    wait(|| {
        for n in client.poll() {
            if let Notice::Review(r) = n {
                summary2 = Some(r);
            }
        }
        summary2.is_some()
    });
    assert!(summary2.unwrap().archived.is_some());
    client
        .approve(
            review_id2,
            BTreeSet::from([Capability::Notifications]),
            false,
        )
        .unwrap();
    wait(|| ready(&client, "test.uninstall"));
    let fresh = f.installer.record("test.uninstall").unwrap().unwrap();
    assert_ne!(fresh.installation_id, record.installation_id);
    assert!(!state_file.exists() && !origin_file.exists());
    assert_eq!(f.setup.vault.get_secret(&scope, "bili.session"), None);

    // 4. Uninstall with keep_data=false wipes the fresh identity's state and secrets immediately.
    let fresh_scope = format!("plugin-v2.{}", fresh.installation_id);
    f.setup
        .vault
        .set_secret(&fresh_scope, "bili.session", Some("sess=2"))
        .unwrap();
    let fresh_state = f
        .setup
        .state_directory
        .join(format!("{}.json", fresh.installation_id));
    std::fs::write(&fresh_state, b"{}").unwrap();
    client.uninstall("test.uninstall", false).unwrap();
    wait(|| {
        client
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::Uninstalled(p) if p == "test.uninstall"))
    });
    assert!(!fresh_state.exists());
    assert_eq!(f.setup.vault.get_secret(&fresh_scope, "bili.session"), None);
    assert!(f.installer.archived("test.uninstall").unwrap().is_none());

    drop((client, binding, summary, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn unavailable_client_emits_error_once_without_hub_state() {
    let (client, _rx) = Client::unavailable(ErrorCode::Internal);
    assert_eq!(client.window_id(), Id(0));
    let first = client.poll();
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first[0],
        Notice::Error {
            plugin: None,
            request: None,
            code: ErrorCode::Internal,
            message: None
        }
    ));
    assert!(client.poll().is_empty());
    assert_eq!(client.reload(), Err(ErrorCode::Cancelled));
    assert_eq!(
        client.advance(Generation {
            media: Id(1),
            ..Generation::default()
        }),
        Err(ErrorCode::Cancelled)
    );
}

#[test]
fn websocket_only_plugin_can_request_and_receive_ws_origin_consent() {
    let mut f = Fixture::new();
    let bytes = package_with(
        "test.ws_origin",
        &[Capability::UiModal, Capability::NetWs],
        r#"{"ok":[{"command":"request_origin","request_id":"77","origin":"ws://192.168.1.50:8080/live"}]}"#,
        true,
    );
    let record = f.install_bytes(&bytes);
    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut review = None;
    wait(|| {
        for notice in client.poll() {
            if let Notice::Origin(value) = notice {
                review = Some(value);
            }
        }
        review.is_some()
    });
    let review = review.unwrap();
    assert_eq!(review.origin, "http://192.168.1.50:8080");
    assert!(review.private && review.plaintext);
    client.confirm_origin(review.id, true, false).unwrap();
    wait(|| {
        client
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::OriginClosed(id) if *id == review.id))
    });
    assert!(
        ready(&client, "test.ws_origin"),
        "NetWs-only plugin must remain Ready after ws:// origin consent"
    );
    let path = f
        .setup
        .state_directory
        .join("origins")
        .join(format!("{}.json", record.installation_id));
    assert!(path.exists());
    drop((review, client, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn overwrite_reinstall_without_keep_identity_scrubs_prior_data_and_object() {
    let mut f = Fixture::new();
    let v1 = package_with(
        "test.overwrite",
        &[Capability::Notifications],
        r#"{"ok":[{"command":"show_toast","message":"v1"}]}"#,
        false,
    );
    let old = f.install_bytes(&v1);
    let old_scope = format!("plugin-v2.{}", old.installation_id);
    f.setup
        .vault
        .set_secret(&old_scope, "token", Some("secret"))
        .unwrap();
    let old_state = f
        .setup
        .state_directory
        .join(format!("{}.json", old.installation_id));
    let old_origin = f
        .setup
        .state_directory
        .join("origins")
        .join(format!("{}.json", old.installation_id));
    std::fs::create_dir_all(old_origin.parent().unwrap()).unwrap();
    std::fs::write(&old_state, b"{}").unwrap();
    std::fs::write(&old_origin, b"{}").unwrap();
    let old_object = f.root.join("packages/objects").join(&old.object);
    assert!(old_object.exists());

    let v2 = package_with(
        "test.overwrite",
        &[Capability::Notifications],
        r#"{"ok":[{"command":"show_toast","message":"v2"}]}"#,
        false,
    );
    let v2_path = f.root.join("v2.irisplugin");
    std::fs::write(&v2_path, &v2).unwrap();

    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    wait(|| ready(&client, "test.overwrite"));
    let review_id = client.review(v2_path).unwrap();
    wait(|| {
        client
            .poll()
            .iter()
            .any(|n| matches!(n, Notice::Review(r) if r.id == review_id))
    });
    client
        .approve(
            review_id,
            BTreeSet::from([Capability::Notifications]),
            false,
        )
        .unwrap();
    wait(|| {
        f.installer
            .record("test.overwrite")
            .unwrap()
            .is_some_and(|r| r.installation_id != old.installation_id)
    });
    wait(|| !old_state.exists() && !old_origin.exists() && !old_object.exists());
    assert_eq!(f.setup.vault.get_secret(&old_scope, "token"), None);
    drop((client, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn reload_with_snapshot_respects_updated_disabled_set() {
    use crate::ui::PluginManager;
    let mut f = Fixture::new();
    f.install("test.toggle");
    let hub = f.hub();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut ui = PluginManager::new(client, None);
    wait(|| {
        ui.poll();
        ui.client()
            .catalog()
            .plugins
            .iter()
            .any(|p| p.id == "test.toggle" && p.status == PluginStatus::Ready)
    });
    ui.reload_with_snapshot(&["test.toggle".into()], PlaybackSnapshot::default());
    wait(|| {
        ui.poll();
        ui.client()
            .catalog()
            .plugins
            .iter()
            .any(|p| p.id == "test.toggle" && p.status == PluginStatus::Disabled)
    });
    ui.reload_with_snapshot(&[], PlaybackSnapshot::default());
    wait(|| {
        ui.poll();
        ui.client()
            .catalog()
            .plugins
            .iter()
            .any(|p| p.id == "test.toggle" && p.status == PluginStatus::Ready)
    });
    drop((ui, hub));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn official_upgrade_with_expanded_capabilities_routes_through_hub_review() {
    let f = Fixture::new();
    let make_official = |version: &str, perms: &[Capability]| {
        let wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 1 4)
            (func (export "iris_abi_version") (result i32) i32.const 2)
            (func (export "iris_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "iris_dealloc") (param i32 i32))
            (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#,
        )
        .unwrap();
        let manifest = serde_json::json!({
            "id": "iris.expand", "name": "Expand", "version": version,
            "protocol": 2, "abi": 2, "entry": "plugin.wasm", "permissions": perms
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
        archive.finish().unwrap().into_inner()
    };

    let b1 = crate::official::Bootstrap::custom(
        f.root.clone(),
        vec![(
            "iris.expand".into(),
            "1".into(),
            make_official("1", &[Capability::UiOverlay]),
        )],
    )
    .unwrap();
    let i1 = Installer::with_bundled(
        f.root.join("packages"),
        f.resources.ledger.clone(),
        b1.pins(),
    );
    let hub1 = Hub::new(Config {
        bootstrap: Some(b1),
        installer: i1.clone(),
        runtime: f.setup.clone(),
        disabled: BTreeSet::new(),
    })
    .unwrap();
    let (c1, _) = hub1.open_window(Generation::default(), None).unwrap();
    wait(|| ready(&c1, "iris.expand"));
    let v1_id = i1.record("iris.expand").unwrap().unwrap().installation_id;
    drop((c1, hub1));
    wait(|| f.resources.ledger.used(None, Resource::ControlStore) == 0);

    let b2 = crate::official::Bootstrap::custom(
        f.root.clone(),
        vec![(
            "iris.expand".into(),
            "2".into(),
            make_official("2", &[Capability::UiOverlay, Capability::Notifications]),
        )],
    )
    .unwrap();
    let i2 = Installer::with_bundled(
        f.root.join("packages"),
        f.resources.ledger.clone(),
        b2.pins(),
    );
    let hub2 = Hub::new(Config {
        bootstrap: Some(b2),
        installer: i2.clone(),
        runtime: f.setup.clone(),
        disabled: BTreeSet::new(),
    })
    .unwrap();
    let (c2, _) = hub2.open_window(Generation::default(), None).unwrap();
    let mut review_id = None;
    wait(|| {
        for n in c2.poll() {
            if let Notice::Review(r) = n
                && r.manifest.id == "iris.expand"
            {
                review_id = Some(r.id);
            }
        }
        review_id.is_some()
    });
    c2.approve(
        review_id.unwrap(),
        BTreeSet::from([Capability::UiOverlay, Capability::Notifications]),
        true,
    )
    .unwrap();
    wait(|| ready(&c2, "iris.expand"));
    let v2 = i2.record("iris.expand").unwrap().unwrap();
    assert_eq!(v2.installation_id, v1_id);
    assert!(v2.grants.contains(&Capability::Notifications));
    drop((c2, hub2));
    wait(|| f.resources.ledger.used(None, Resource::HostBytes) == 0);
}

mod control_recovery;
