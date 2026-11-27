use super::*;
use crate::actor::Status;
use crate::installation::Approval;
use crate::package::{Package, Provenance};
use iris_api::v2::{Capability, NetworkDeclaration, source::PlaybackSnapshot};
use iris_core::vault::CredentialVault;
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use zip::{ZipWriter, write::SimpleFileOptions};

fn package(reply: &str, permissions: Vec<Capability>) -> Vec<u8> {
    let data = reply
        .as_bytes()
        .iter()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let output = (2048_u64 << 32) | reply.len() as u64;
    let wasm = wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 4 8)
        (data (i32.const 2048) "{data}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const {output}))"#
    ))
    .unwrap();
    let manifest = serde_json::json!({"id":"fixture.runtime","name":"Runtime","version":"1","protocol":2,"abi":2,"entry":"plugin.wasm","permissions":permissions});
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("plugin.json", serde_json::to_vec(&manifest).unwrap()),
        ("plugin.wasm", wasm),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
struct Fixture {
    root: PathBuf,
    installer: Installer,
    installed: Installed,
    setup: Setup,
}
impl Fixture {
    fn new(reply: &str, grants: Vec<Capability>) -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-v2-runtime-{}-{}",
            std::process::id(),
            next_id().unwrap()
        ));
        let resources = Resources::new().unwrap();
        let setup = Setup {
            resources: resources.clone(),
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        };
        let mut installer = Installer::new(root.join("packages"), resources.ledger.clone());
        let bytes = package(reply, grants.clone());
        let digest = Package::from_zip(&bytes, Provenance::User).unwrap().digest;
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
            &CancelToken::new(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn controls_share_global_budget_and_single_window_media_lifetime_tracks_reopen() {
    let f = Fixture::new(r#"{"ok":[]}"#, vec![Capability::MediaSource]);
    let mut control = f.load();
    wait(|| control.status() == Status::Ready);
    assert!(
        Control::load(
            &f.installer,
            f.installed.clone(),
            f.setup.clone(),
            &CancelToken::new()
        )
        .is_err()
    );
    let a = Control::new_window_id().unwrap();
    let b = Control::new_window_id().unwrap();
    let gen_a = Generation {
        media: Id(1),
        ..Generation::default()
    };
    let browser: crate::auth::BrowserOpen = Arc::new(|_, _, _| Err(ErrorCode::Denied));
    let plan = control
        .window_preparation(a, gen_a, None, browser.clone())
        .unwrap();
    control.attach_window(plan.prepare().unwrap()).unwrap();
    assert_eq!(
        control
            .window_preparation(b, gen_a, None, browser.clone())
            .err(),
        Some(ErrorCode::Backpressure)
    );
    let media_a = control.window.as_ref().unwrap().1.media.clone();
    control
        .advance_window(
            a,
            Generation {
                seek: Id(1),
                ..gen_a
            },
        )
        .unwrap();
    assert!(
        !media_a.is_cancelled(),
        "seeking must not stop the admitted relay"
    );
    control
        .advance_window(
            a,
            Generation {
                media: Id(2),
                ..Generation::default()
            },
        )
        .unwrap();
    assert!(media_a.is_cancelled());
    control.close_window(a).unwrap();

    let plan_b = control.window_preparation(b, gen_a, None, browser).unwrap();
    control.attach_window(plan_b.prepare().unwrap()).unwrap();
    let media_b = control.window.as_ref().unwrap().1.media.clone();
    assert!(control.playback_permit(b, gen_a).is_ok());
    control.stop();
    assert!(media_b.is_cancelled());
    wait(|| control.is_quiescent());
    drop(control);
    wait(|| f.setup.resources.ledger.used(None, Resource::HostBytes) == 0);
    assert_eq!(f.setup.resources.ledger.used(None, Resource::WasmBytes), 0);
}

#[test]
fn global_callback_cannot_issue_window_playback_commands() {
    let f = Fixture::new(
        r#"{"ok":[{"command":"seek_absolute","seconds":5.0}]}"#,
        vec![Capability::ControlPlayback],
    );
    let mut control = f.load();
    wait(|| {
        control.poll();
        matches!(control.status(), Status::Failed(ErrorCode::Denied))
    });
    assert!(control.stopped);
    wait(|| control.is_quiescent());
}

#[test]
fn read_state_filter_never_leaks_local_snapshot_through_other_events() {
    let snapshot = PlaybackSnapshot {
        uri: Some("file:///private.mov".into()),
        title: Some("private-title".into()),
        ..PlaybackSnapshot::default()
    };
    let caps = BTreeSet::from([Capability::UiOverlay]);
    let event = HostEvent::Command {
        id: "test".into(),
        snapshot: Some(snapshot.clone()),
    };
    let Some(HostEvent::Command { snapshot, .. }) = events::filter(event, &caps, &[]).unwrap()
    else {
        panic!();
    };
    assert!(snapshot.is_none());
    assert!(
        events::filter(
            HostEvent::StateChanged {
                generation: Generation::default(),
                snapshot: PlaybackSnapshot {
                    uri: Some("file:///private.mov".into()),
                    ..PlaybackSnapshot::default()
                }
            },
            &caps,
            &[]
        )
        .unwrap()
        .is_none()
    );
    assert!(
        events::filter(
            HostEvent::Tick {
                generation: Generation::default(),
                position: 2.0,
                duration: 4.0,
                paused: false
            },
            &caps,
            &[]
        )
        .unwrap()
        .is_some()
    );
    assert!(
        events::filter(
            HostEvent::SourceBrowse {
                request_id: Id(1),
                source_id: "other-plugin-source".into(),
                folder_id: None,
                page: None,
                query: None,
                filters: BTreeMap::new()
            },
            &BTreeSet::from([Capability::MediaSource]),
            &[]
        )
        .is_err()
    );
}

#[test]
fn altered_approval_cannot_be_loaded_against_an_existing_record() {
    let f = Fixture::new(r#"{"ok":[]}"#, vec![Capability::MediaSource]);
    let mut changed = f.installed.clone();
    changed.grants.insert(Capability::Secrets);
    assert!(matches!(
        f.installer.load(&changed),
        Err(ErrorCode::StaleHandle)
    ));
    let record = f.root.join("packages/installed/fixture.runtime.json");
    std::fs::write(&record, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(matches!(f.installer.load(&changed), Err(ErrorCode::Denied)));
}

#[test]
fn origin_granted_filter_accepts_net_tcp_only_with_ui_modal() {
    let event = || HostEvent::OriginGranted {
        request_id: Id(1),
        origin: "http://192.168.1.61:9000".into(),
        grant: Some(Id(7)),
        insecure_auth: false,
    };
    assert!(
        events::filter(
            event(),
            &BTreeSet::from([Capability::UiModal, Capability::NetTcp]),
            &[],
        )
        .unwrap()
        .is_some()
    );
    assert_eq!(
        events::filter(event(), &BTreeSet::from([Capability::NetTcp]), &[]).err(),
        Some(ErrorCode::Denied)
    );
    assert_eq!(
        events::filter(event(), &BTreeSet::from([Capability::UiModal]), &[]).err(),
        Some(ErrorCode::Denied)
    );
}

#[test]
fn attach_window_rollback_on_saturated_actor_allows_clean_retry_with_same_window_id() {
    let f = Fixture::new(r#"{"ok":[]}"#, vec![Capability::Notifications]);
    let mut control = f.load();
    wait(|| control.status() == Status::Ready);

    let gen0 = Generation::default();
    let browser: crate::auth::BrowserOpen = Arc::new(|_, _, _| Err(ErrorCode::Denied));

    // Exhaust all but 1 Handle for this owner so `services.open_window` succeeds while
    // `actor.open_window_with_event` fails inside `attach_window`, then verify retrying
    // the exact same `second` window Id succeeds cleanly without leaking retired scopes.
    let base_used = f
        .setup
        .resources
        .ledger
        .used(Some(&f.installed.installation_id), Resource::Handle);
    let hold = f
        .setup
        .resources
        .ledger
        .reserve(
            &f.installed.installation_id,
            &[(Resource::Handle, 128 - base_used - 1)],
        )
        .unwrap();

    let second = Control::new_window_id().unwrap();
    for _ in 0..9 {
        let plan = control
            .window_preparation(second, gen0, None, browser.clone())
            .unwrap();
        assert_eq!(
            control.attach_window(plan.prepare().unwrap()),
            Err(ErrorCode::QuotaExceeded)
        );
    }
    drop(hold);

    let retry_plan = control
        .window_preparation(second, gen0, None, browser)
        .unwrap();
    control
        .attach_window(retry_plan.prepare().unwrap())
        .unwrap();

    control.stop();
    wait(|| control.is_quiescent());
    drop(control);
    wait(|| f.setup.resources.ledger.used(None, Resource::HostBytes) == 0);
}
