use super::*;
use iris_api::v2::{HostEvent, Id, NetworkDeclaration, source::PlaybackSnapshot};
use iris_core::vault::CredentialVault;
use iris_plugin_host::{
    installation::{Approval, Installed, Installer},
    package::{Package, Provenance},
    runtime::{Control, Resources, Setup},
};
use std::{
    io::{Cursor, Write},
    path::PathBuf,
};
use zip::{ZipWriter, write::SimpleFileOptions};

struct Fixture {
    root: PathBuf,
    installer: Installer,
    installed: Installed,
    setup: Setup,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-runtime-guest-{}-{}",
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
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let manifest = serde_json::to_vec(&serde_json::json!({"id":"test.runtime","name":"Rust runtime","version":"1","protocol":2,"abi":2,"entry":"plugin.wasm","permissions":["notifications","media_source"]})).unwrap();
        for (name, bytes) in [
            ("plugin.json", manifest),
            ("plugin.wasm", guest("IRIS_V2_RUST_GUEST")),
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
                    grants: BTreeSet::from([Capability::Notifications, Capability::MediaSource]),
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
        let mut control = Control::load(
            &self.installer,
            self.installed.clone(),
            self.setup.clone(),
            &CancelToken::new(),
        )
        .unwrap();
        until(
            || {
                control.poll();
                control.status() == Status::Ready && control.sources().len() == 1
            },
            Duration::from_secs(5),
        );
        control
    }
    fn open(&self, control: &mut Control) -> Id {
        let id = Control::new_window_id().unwrap();
        let prepared = control
            .window_preparation(
                id,
                Generation::default(),
                None,
                Arc::new(|_, _, _| Err(ErrorCode::Denied)),
            )
            .unwrap()
            .prepare()
            .unwrap();
        control.attach_window(prepared).unwrap();
        id
    }
    fn command(&self, control: &mut Control, window: Id, command: &str) -> String {
        control
            .send(
                window,
                HostEvent::Command {
                    id: command.into(),
                    snapshot: Some(PlaybackSnapshot {
                        uri: Some("file:///must-remain-private.mov".into()),
                        ..PlaybackSnapshot::default()
                    }),
                },
            )
            .unwrap();
        let mut reply = None;
        until(
            || {
                reply = control.poll();
                reply.is_some()
            },
            Duration::from_secs(4),
        );
        let reply = reply.unwrap();
        assert_eq!(reply.window, Some(window));
        let [HostCommand::ShowToast { message }] = reply.commands.as_slice() else {
            panic!("expected fixture result");
        };
        message.clone()
    }
    fn stop(&self, mut control: Control) {
        control.stop();
        until(|| control.is_quiescent(), Duration::from_secs(4));
        drop(control);
        until(
            || self.setup.resources.ledger.used(None, Resource::HostBytes) == 0,
            Duration::from_secs(4),
        );
        assert_eq!(
            self.setup
                .resources
                .ledger
                .used(None, Resource::ControlStore),
            0
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn installed_guest_persists_state_without_read_state_and_cannot_receive_local_snapshot() {
    let fixture = Fixture::new();
    let mut control = fixture.load();
    let window = fixture.open(&mut control);
    assert_eq!(fixture.command(&mut control, window, "snapshot"), "none");
    assert_eq!(
        fixture.command(&mut control, window, "store_read"),
        "absent"
    );
    assert_eq!(
        fixture.command(&mut control, window, "store_write"),
        "persistent-value"
    );
    fixture.stop(control);
    let mut reopened = fixture.load();
    let next_window = fixture.open(&mut reopened);
    assert_eq!(
        fixture.command(&mut reopened, next_window, "store_read"),
        "persistent-value"
    );
    fixture.stop(reopened);
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn admitted_playback_survives_request_cancel_and_control_timeout_until_explicit_stop() {
    use iris_net::http_client::HttpCancel;
    let fixture = Fixture::new();
    let mut control = fixture.load();
    let window = fixture.open(&mut control);
    control
        .send(
            window,
            HostEvent::SourceResolve {
                request_id: Id(100),
                source_id: "fixture.source".into(),
                folder_id: None,
                entry_id: "av".into(),
                variant: None,
            },
        )
        .unwrap();
    let mut update = None;
    until(
        || {
            update = control.poll();
            update.is_some()
        },
        Duration::from_secs(3),
    );
    let update = update.unwrap();
    let [HostCommand::ResolveStream { request_id, media }] = update.commands.as_slice() else {
        panic!("missing resolution");
    };
    assert_eq!(*request_id, Id(100));
    let request_cancel = HttpCancel::default();
    let playback = control
        .playback_permit(window, Generation::default())
        .unwrap()
        .prepare(&media.stream, &update.blobs, &request_cancel)
        .unwrap();
    drop(update);
    request_cancel.cancel();
    let path = url::Url::parse(&playback.url).unwrap().path().to_owned();
    let read = |offset: u64| {
        let access = playback
            .relay
            .session()
            .open(&path, &HttpCancel::default())
            .unwrap();
        let read = access.reader.read_at(offset, 16, &access.cancel).unwrap();
        assert_eq!(
            read.blob.bytes,
            (0..16)
                .map(|n| (offset + n) as u8 ^ 0xa5)
                .collect::<Vec<_>>()
        );
    };
    read(5);
    control
        .send(
            window,
            HostEvent::Command {
                id: "spin".into(),
                snapshot: None,
            },
        )
        .unwrap();
    until(
        || {
            control.poll();
            control.status() == Status::Failed(ErrorCode::Timeout)
        },
        Duration::from_secs(4),
    );
    read(40);
    assert!(matches!(
        control.playback_permit(window, Generation::default()),
        Err(ErrorCode::Timeout)
    ));
    control.stop();
    assert!(matches!(
        playback.relay.session().open(&path, &HttpCancel::default()),
        Err(ErrorCode::Cancelled)
    ));
    until(|| playback.relay.is_quiescent(), Duration::from_secs(4));
    drop(playback);
    fixture.stop(control);
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn coordinator_routes_live_source_requests_and_stops_stream_on_window_close() {
    use iris_net::http_client::HttpCancel;
    use iris_plugin_host::hub::{Config, Hub, Notice};
    let fixture = Fixture::new();
    let hub = Hub::new(Config {
        bootstrap: None,
        installer: fixture.installer.clone(),
        runtime: fixture.setup.clone(),
        disabled: BTreeSet::new(),
    })
    .unwrap();
    let (first, _) = hub.open_window(Generation::default(), None).unwrap();
    assert!(matches!(
        hub.open_window(Generation::default(), None),
        Err(ErrorCode::Backpressure)
    ));
    until(|| first.is_ready("test.runtime"), Duration::from_secs(60));
    assert_eq!(
        fixture
            .setup
            .resources
            .ledger
            .used(None, Resource::ControlStore),
        1
    );
    assert_eq!(
        first.catalog().plugins[0].sources.as_ref().unwrap().sources[0].id,
        "fixture.source"
    );
    assert!(
        matches!(
            first.send(
                "test.runtime",
                HostEvent::SourceResolve {
                    request_id: Id(999),
                    source_id: "fixture.source".into(),
                    folder_id: None,
                    entry_id: "av".into(),
                    variant: None,
                }
            ),
            Err(ErrorCode::Denied)
        ),
        "a UI-supplied ID cannot bypass the source request table"
    );
    let request = first
        .resolve("test.runtime", "fixture.source", None, "av", None)
        .unwrap();
    let mut playback = None;
    until(
        || {
            for notice in first.poll() {
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
        Duration::from_secs(5),
    );
    let playback = playback.unwrap();
    let path = url::Url::parse(&playback.url).unwrap().path().to_owned();
    let access = playback
        .relay
        .session()
        .open(&path, &HttpCancel::default())
        .unwrap();
    let read = access.reader.read_at(5, 16, &access.cancel).unwrap();
    assert_eq!(
        read.blob.bytes,
        (5..21).map(|n| n ^ 0xa5).collect::<Vec<u8>>()
    );
    drop((read, access));
    first.shutdown();
    until(
        || playback.relay.session().cancellation().is_cancelled(),
        Duration::from_secs(3),
    );
    until(|| playback.relay.is_quiescent(), Duration::from_secs(4));
    drop((playback, first, hub));
    until(
        || {
            fixture
                .setup
                .resources
                .ledger
                .used(None, Resource::HostBytes)
                == 0
        },
        Duration::from_secs(4),
    );
}
