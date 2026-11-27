use super::enhance::EnhancementState;
use super::{
    ActionKind, EnhancementOutcome, POLL_ACTION_LIMIT, REMOTE_THUMB_LEASE_BYTES, ReadyEffect,
    RetainedThumbnail, WindowSession,
};
use crate::{
    hub::{Config, Hub},
    installation::{Approval, Installer},
    ledger::{Ledger, Resource},
    package::{Package, Provenance},
    runtime::{Resources, Setup},
};
use futures::StreamExt;
use iris_api::v2::{Capability, ErrorCode, Generation, HostEvent, Id};
use iris_core::vault::CredentialVault;
use std::{
    collections::BTreeSet,
    io::{Cursor, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("iris-ws-test-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[track_caller]
fn wait(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting in WindowSession test"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn build_plugin_zip(id: &str, perms: &[Capability], wat_src: &str) -> Vec<u8> {
    let wasm = wat::parse_str(wat_src).expect("valid WAT");
    let manifest = serde_json::json!({
        "id": id,
        "name": id,
        "version": "0.1.0",
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": perms,
        "commands": [{"id": "go", "title": "Go"}]
    });
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("plugin.json", opts).unwrap();
    zip.write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    zip.start_file("plugin.wasm", opts).unwrap();
    zip.write_all(&wasm).unwrap();
    zip.finish().unwrap().into_inner()
}

fn single_reply_wat(reply_json: &str) -> String {
    let data = reply_json
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    format!(
        r#"(module
            (memory (export "memory") 4 8)
            (data (i32.const 2048) "{data}")
            (data (i32.const 16384) "\7b\22ok\22:[]\7d")
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
                if (result i64) i64.const {window_reply} else i64.const {empty_reply} end)
        )"#,
        window_reply = (2048_u64 << 32) | reply_json.len() as u64,
        empty_reply = (16384_u64 << 32) | 9,
        first = u64::from_le_bytes(*b"\"event\":"),
        second = u64::from_le_bytes(*b"\"window\"")
    )
}

#[test]
fn clearing_lut_rejects_late_preparation_with_or_without_an_applied_filter() {
    for applied_owner in [None, Some("plugin"), Some("other")] {
        let mut state = EnhancementState::new(Generation::default());
        state.lut_owner = applied_owner.map(str::to_owned);
        let key = ("plugin".into(), "lut".into());
        let other = ("other".into(), "lut".into());
        let track = ("plugin".into(), "track:subtitle".into());
        state.pending.insert(key.clone(), Id(1));
        state.pending.insert(other.clone(), Id(2));
        state.pending.insert(track.clone(), Id(3));

        assert_eq!(state.cancel_lut("plugin"), applied_owner == Some("plugin"));
        assert!(
            !state.finish_job(&key, Id(1)),
            "cleared LUT must never apply"
        );
        // Keep native ownership until the caller successfully clears the filter.
        assert_eq!(state.lut_owner.as_deref(), applied_owner);
        assert!(state.finish_job(&other, Id(2)));
        assert!(state.finish_job(&track, Id(3)));
    }
}

#[test]
fn a_late_cleared_lut_cannot_consume_a_newer_preparation() {
    let mut state = EnhancementState::new(Generation::default());
    let key = ("plugin".into(), "lut".into());
    state.pending.insert(key.clone(), Id(1));
    assert!(!state.cancel_lut("plugin"));
    state.pending.insert(key.clone(), Id(2));
    assert!(!state.finish_job(&key, Id(1)));
    assert!(state.finish_job(&key, Id(2)));
    assert!(!state.finish_job(&key, Id(2)));
}

#[test]
fn retained_thumbnail_releases_lease_on_none_and_holds_until_drop_on_some() {
    let ledger = Ledger::default();
    let lease_none = ledger
        .reserve("owner", &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)])
        .unwrap();
    let neg: RetainedThumbnail<u32> = RetainedThumbnail::new(None, lease_none);
    assert!(neg.image.is_none());
    assert_eq!(ledger.used(Some("owner"), Resource::HostBytes), 0);

    let lease_some = ledger
        .reserve("owner", &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)])
        .unwrap();
    let pos = RetainedThumbnail::new(Some(7u32), lease_some);
    assert_eq!(
        ledger.used(Some("owner"), Resource::HostBytes),
        REMOTE_THUMB_LEASE_BYTES
    );
    drop(pos);
    assert_eq!(ledger.used(Some("owner"), Resource::HostBytes), 0);
}

#[test]
fn bounded_poll_enforces_action_limits_rearms_wake_and_drops_stale_generations() {
    let dir = TestDir::new("bounded-poll");
    let resources = Resources::new().unwrap();
    let mut installer = Installer::new(dir.0.join("packages"), resources.ledger.clone());

    // Emit 40 `TogglePlayback` commands in one guest response (> POLL_ACTION_LIMIT = 32).
    let reply = serde_json::to_string(&iris_api::v2::Reply::Ok {
        ok: vec![iris_api::v2::HostCommand::TogglePlayback; 40],
    })
    .unwrap();
    let zip_bytes = build_plugin_zip(
        "test.burst",
        &[Capability::ControlPlayback],
        &single_reply_wat(&reply),
    );
    let pkg = Package::from_zip(&zip_bytes, Provenance::User).unwrap();
    installer
        .install(
            &zip_bytes,
            Approval {
                digest: pkg.digest,
                grants: pkg.manifest.permissions.into_iter().collect(),
                network: pkg.manifest.network,
                previous: None,
            },
        )
        .unwrap();

    let hub = Hub::new(Config {
        installer,
        bootstrap: None,
        disabled: BTreeSet::new(),
        runtime: Setup {
            resources: resources.clone(),
            state_directory: dir.0.join("state"),
            temporary_directory: dir.0.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        },
    })
    .unwrap();
    let (client, mut wake_rx) = hub.open_window(Generation::default(), None).unwrap();
    let mut session = WindowSession::new(client, None);

    // `WindowOpened` emits 40 `toggle_playback` commands (> POLL_ACTION_LIMIT = 32).
    let mut first_batch = 0;
    wait(|| {
        let r = session.poll();
        if !r.actions.is_empty() {
            first_batch = r.actions.len();
            true
        } else {
            false
        }
    });
    assert_eq!(first_batch, POLL_ACTION_LIMIT);
    // Remaining 8 commands stayed in Inbox and re-armed `wake_rx`!
    assert!(
        futures::executor::block_on(async { futures::poll!(wake_rx.next()) }).is_ready(),
        "bounded poll must re-arm wake channel when unread commands remain"
    );
    let second = session.poll();
    assert_eq!(second.actions.len(), 40 - POLL_ACTION_LIMIT);
    assert!(second.actions.iter().all(|a| matches!(
        a.kind,
        ActionKind::Player(iris_core::player::Command::TogglePlayback)
    )));
    while wake_rx.try_recv().is_ok() {}

    // Trigger another burst, wait for wake notification (commands queued in Inbox),
    // then advance media BEFORE polling -> stale generation must be dropped!
    let sent = session.send_to(
        "test.burst",
        HostEvent::Command {
            id: "go".into(),
            snapshot: None,
        },
    );
    assert!(sent.notices.is_empty());
    wait(|| wake_rx.try_recv().is_ok());
    session.advance_media().unwrap();
    let after_advance = session.poll();
    assert!(
        after_advance.actions.is_empty(),
        "commands emitted for superseded media generation must be dropped"
    );

    drop((second, after_advance, session, hub));
    wait(|| resources.ledger.used(None, Resource::HostBytes) == 0);
}

#[test]
fn window_session_registers_and_clears_active_stream_on_open_failure_switch_stop_and_close() {
    let dir = TestDir::new("stream-lifecycle");
    let resources = Resources::new().unwrap();
    let mut installer = Installer::new(dir.0.join("packages"), resources.ledger.clone());

    let timeline_reply = serde_json::to_string(&iris_api::v2::Reply::Ok {
        ok: vec![iris_api::v2::HostCommand::SetTimelineMeta {
            media: Id(0),
            generation: Generation::default(),
            chapters: vec![iris_api::v2::enhance::Chapter {
                start_ms: 0.into(),
                title: "Intro".into(),
            }],
            heat: Vec::new(),
            skip: Vec::new(),
        }],
    })
    .unwrap();
    let noop_wat = single_reply_wat(&timeline_reply);
    let zip_bytes = build_plugin_zip(
        "test.stream",
        &[Capability::MediaSource, Capability::MediaEnhance],
        &noop_wat,
    );
    let pkg = Package::from_zip(&zip_bytes, Provenance::User).unwrap();
    installer
        .install(
            &zip_bytes,
            Approval {
                digest: pkg.digest,
                grants: pkg.manifest.permissions.into_iter().collect(),
                network: pkg.manifest.network,
                previous: None,
            },
        )
        .unwrap();
    let record = installer.record("test.stream").unwrap().unwrap();
    let hub = Hub::new(Config {
        installer,
        bootstrap: None,
        disabled: BTreeSet::new(),
        runtime: Setup {
            resources: resources.clone(),
            state_directory: dir.0.join("state"),
            temporary_directory: dir.0.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        },
    })
    .unwrap();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut ws = WindowSession::new(client, None);
    let mut timeline_req = None;
    wait(|| {
        let r = ws.poll();
        for act in r.actions {
            if let ActionKind::Enhancement(req) = act.kind {
                timeline_req = Some(req);
            }
        }
        ws.is_ready("test.stream") && timeline_req.is_some()
    });

    // Execute timeline enhancement and verify ReadyEffect::Timeline ownership handle.
    let outcome = ws.execute_enhancement(timeline_req.unwrap(), true, 0, || {
        Err(iris_core::store::AnnotationError::Denied)
    });
    match outcome {
        EnhancementOutcome::Immediate(ReadyEffect::Timeline(lease)) => {
            assert_eq!(lease.chapters().len(), 1);
            ws.commit_timeline(lease);
            assert!(ws.timeline_ranges().is_some());
        }
        _ => panic!("expected Immediate(ReadyEffect::Timeline)"),
    }

    let make_playback = |generation: Generation| {
        // Dropping a prior Playback/ScopedPlayback wakes its BoundRelay listener thread
        // asynchronously; wait for that listener to exit before reserving a new session.
        wait(|| resources.ledger.used(None, Resource::PlaybackSession) == 0);
        let grant = crate::network::NetworkGrant::new(
            record.installation_id.clone(),
            BTreeSet::new(),
            Default::default(),
            &resources.ledger,
        )
        .unwrap();
        let factory =
            crate::stream::remote::RemoteFactory::new(grant, resources.ledger.clone()).unwrap();
        let bound = crate::stream::relay::BoundRelay::bind().unwrap();
        let stream_session = crate::stream::session::StreamSession::new(
            crate::stream::session::new_session_id().unwrap(),
            generation,
            bound.authority().unwrap(),
            factory,
            None,
        )
        .unwrap();
        let reader = Arc::new(
            crate::stream::reader::MemoryReader::new(
                vec![0u8; 32],
                &record.installation_id,
                &resources.ledger,
            )
            .unwrap(),
        );
        stream_session
            .commit(stream_session.prepare_reader(reader).unwrap())
            .unwrap();
        let url = stream_session.root_url().unwrap();
        crate::stream::prepare::Playback {
            relay: bound.start(stream_session).unwrap(),
            url,
            profile: None,
            kind: iris_core::library::Kind::Audio,
            title: "Clip".into(),
            source_id: Some("src".into()),
            entry_id: Some("ep".into()),
            is_live: false,
            attachments: Vec::new(),
            variants: Vec::new(),
            active_variant: None,
        }
    };

    // 1. Stale generation admission is rejected immediately.
    let stale_pb = make_playback(Generation {
        media: Id(99),
        seek: Id(0),
        context: Id(0),
    });
    assert!(matches!(
        ws.admit_playback(
            "test.stream",
            Generation {
                media: Id(99),
                seek: Id(0),
                context: Id(0),
            },
            stale_pb,
            |_| Ok::<_, &str>(())
        ),
        Err(ErrorCode::StaleHandle)
    ));
    assert!(!ws.has_active_stream_for(None));

    // 2. Open failure clears active stream and cancels StreamSession immediately.
    let fail_pb = make_playback(ws.generation());
    let fail_res = ws
        .admit_playback("test.stream", ws.generation(), fail_pb, |_| {
            Err::<(), _>("open failed")
        })
        .unwrap();
    assert!(fail_res.is_err());
    assert!(!ws.has_active_stream_for(None));

    // 3. Successful admission registers active stream; explicit stop (`clear_active_stream`) clears it.
    let ok_pb = make_playback(ws.generation());
    let mut kept_scoped = None;
    ws.admit_playback("test.stream", ws.generation(), ok_pb, |scoped| {
        kept_scoped = Some(scoped);
        Ok::<_, &str>(())
    })
    .unwrap()
    .unwrap();
    assert!(ws.has_active_stream_for(None));
    assert!(ws.has_active_stream_for(Some("test.stream")));
    assert!(!ws.has_active_stream_for(Some("other.plugin")));
    ws.clear_active_stream();
    assert!(!ws.has_active_stream_for(None));
    drop(kept_scoped);

    // 4. Media switch (`advance_media`) clears active stream and resets timeline.
    let switch_pb = make_playback(ws.generation());
    let mut switch_scoped = None;
    ws.admit_playback("test.stream", ws.generation(), switch_pb, |scoped| {
        switch_scoped = Some(scoped);
        Ok::<_, &str>(())
    })
    .unwrap()
    .unwrap();
    assert!(ws.has_active_stream_for(Some("test.stream")));
    ws.advance_media().unwrap();
    assert!(!ws.has_active_stream_for(None));
    assert!(ws.timeline_ranges().is_none());
    drop(switch_scoped);

    // 5. Window close (`shutdown`) cancels & clears active stream.
    let close_pb = make_playback(ws.generation());
    let mut close_scoped = None;
    ws.admit_playback("test.stream", ws.generation(), close_pb, |scoped| {
        close_scoped = Some(scoped);
        Ok::<_, &str>(())
    })
    .unwrap()
    .unwrap();
    assert!(ws.has_active_stream_for(Some("test.stream")));
    ws.shutdown();
    assert!(!ws.has_active_stream_for(None));
    assert!(close_scoped.unwrap().stream.is_cancelled());

    drop((ws, hub));
    wait(|| {
        resources.ledger.used(None, Resource::HostBytes) == 0
            && resources.ledger.used(None, Resource::PlaybackSession) == 0
    });
    assert_eq!(resources.ledger.used(None, Resource::PlaybackSession), 0);
}

#[test]
fn attachment_preparation_enforces_format_owner_quota_and_holds_ledger_until_external_track_drop() {
    use crate::{
        actor::{Executor, Operation, TaskContext},
        enhancement::{AttachmentRegistry, AttachmentSpec},
        files::{FileExecutor, FileSetup},
        instance::Blob,
        network::NetworkGrant,
        runtime::WindowAccess,
    };
    use iris_api::v2::{
        NetworkDeclaration, Role,
        enhance::{EnhancementResource, SubtitleFormat},
        task::{ArtifactFormat, Task},
    };
    use iris_base::net::http_client::HttpCancel;
    use iris_core::player::ExternalTrack;
    use std::collections::BTreeMap;

    let root = TestDir::new("host-attach-contract");
    let resources = Resources::new().unwrap();
    let ledger = resources.ledger.clone();
    let caps = BTreeSet::from([Capability::MediaEnhance]);
    let network = NetworkGrant::new(
        "enhance-test".into(),
        caps.clone(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let files = FileExecutor::new(FileSetup {
        network: network.clone(),
        ledger: ledger.clone(),
        capabilities: caps.clone(),
        temporary_directory: root.0.clone(),
    })
    .unwrap();
    let access = WindowAccess {
        instance: Id(1),
        window: Id(2),
        generation: Generation::default(),
        owner: "enhance-test".into(),
        capabilities: caps,
        network,
        files,
        resources,
        cancel: HttpCancel::default(),
        attachments: Arc::new(AttachmentRegistry::default()),
    };
    let run = |task, blobs: &BTreeMap<u64, Arc<Blob>>| {
        let context = TaskContext {
            instance_id: access.instance,
            window: Some(access.window),
            owner: access.owner.clone(),
            role: Role::Control,
            generation: access.generation,
            cancel: access.cancel.clone(),
            ledger: ledger.clone(),
        };
        match access.files.prepare(task, blobs, context).unwrap() {
            Operation::Blocking(job) => job().unwrap(),
            _ => panic!("expected blocking artifact job"),
        }
    };
    let created = run(
        Task::ArtifactBegin {
            format: ArtifactFormat::WebVtt,
        },
        &BTreeMap::new(),
    );
    let writer = Id(created.value["artifact_writer"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    let vtt = b"WEBVTT\n\n00:00.000 --> 00:03.000\nReceipt\n".to_vec();
    let blobs = BTreeMap::from([(9, Arc::new(Blob::new(&ledger, &access.owner, vtt).unwrap()))]);
    let written = run(
        Task::ArtifactWrite {
            handle: writer,
            blob: Id(9),
            finish: true,
        },
        &blobs,
    );
    let artifact = Id(written.value["artifact"].as_str().unwrap().parse().unwrap());
    let spec = || AttachmentSpec {
        id: "subtitle".into(),
        label: "Receipt".into(),
        lang: None,
        subtitle: Some(SubtitleFormat::WebVtt),
        resource: EnhancementResource::Artifact { handle: artifact },
    };

    // 1. Format mismatch -> InvalidArgument.
    let mut wrong = spec();
    wrong.subtitle = Some(SubtitleFormat::Ass);
    assert!(matches!(
        access.prepare_attachment(wrong),
        Err(ErrorCode::InvalidArgument)
    ));

    // 2. Installation owner mismatch -> Denied.
    let mut unauthorized = access.clone();
    unauthorized.owner = "another-installation".into();
    assert!(matches!(
        unauthorized.prepare_attachment(spec()),
        Err(ErrorCode::Denied)
    ));
    drop(unauthorized);

    // 3. Per-plugin 8-track quota -> 9th rejected with QuotaExceeded.
    let track: Arc<dyn ExternalTrack> = access.prepare_attachment(spec()).unwrap();
    let extra: Vec<_> = (0..7)
        .map(|_| access.prepare_attachment(spec()).unwrap())
        .collect();
    assert!(matches!(
        access.prepare_attachment(spec()),
        Err(ErrorCode::QuotaExceeded)
    ));
    drop(extra);
    assert_eq!(
        ledger.used(Some(&access.owner), Resource::EnhancementTrack),
        1
    );

    // 4. Simulated native receipt queue holds EnhancementTrack reservation after revoke until dropped.
    let mut in_flight_receipts: Vec<Arc<dyn ExternalTrack>> = vec![track.clone()];
    drop(track);
    assert_eq!(ledger.used(None, Resource::EnhancementTrack), 1);
    in_flight_receipts.clear();
    assert_eq!(ledger.used(None, Resource::EnhancementTrack), 0);

    access.cancel.cancel();
    access.files.stop();
    drop((created, written, blobs));
    wait(|| access.files.is_quiescent());
    drop(access);
    wait(|| {
        ledger.used(None, Resource::HostBytes) == 0 && ledger.used(None, Resource::TempBytes) == 0
    });
}

#[test]
fn monitor_lut_and_scoped_stream_ports_hold_ledger_until_consumer_releases_handles() {
    use crate::{
        files::TempRoot,
        lut::MonitorLut,
        network::NetworkGrant,
        stream::{
            prepare::{AttachmentView, Playback},
            reader::MemoryReader,
            relay::BoundRelay,
            remote::RemoteFactory,
            session::{StreamSession, new_session_id},
        },
    };
    use iris_api::v2::NetworkDeclaration;
    use iris_base::net::http_client::HttpCancel;
    use iris_core::{
        library::Kind,
        player::{MonitorLutFilter, ScopedPlayback},
    };

    // 1. Rapid MonitorLut replacements held by a simulated native receipt queue keep TempBytes
    // reserved until all retired `Arc<dyn MonitorLutFilter>` handles drop.
    let lut_dir = TestDir::new("lut-[,;' port");
    let ledger = Ledger::default();
    let root = TempRoot::new(&lut_dir.0, "lut-port".into(), ledger.clone()).unwrap();
    let cancel = HttpCancel::default();
    let make_lut = |rgb: &str| -> Arc<dyn MonitorLutFilter> {
        let mut writer = root.writer("cube").unwrap();
        let cube = format!("LUT_3D_SIZE 2\n{}", format!("{rgb}\n").repeat(8));
        writer.write(cube.as_bytes(), &cancel).unwrap();
        MonitorLut::prepare(writer.finish(&cancel).unwrap(), &ledger, &cancel).unwrap()
    };
    let mut pending_native_replies = vec![make_lut("1 0 0"), make_lut("0 1 0"), make_lut("0 0 1")];
    assert!(ledger.used(None, Resource::TempBytes) > 0);
    pending_native_replies.clear();
    wait(|| {
        root.is_quiescent()
            && ledger.used(None, Resource::HostBytes) == 0
            && ledger.used(None, Resource::TempBytes) == 0
    });
    drop(root);

    // 2. `Playback::into_scoped` exercises the `ScopedStream` port without `Session`.
    let grant = NetworkGrant::new(
        "scoped-port".into(),
        BTreeSet::new(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let factory = RemoteFactory::new(grant.clone(), ledger.clone()).unwrap();
    let bound = BoundRelay::bind().unwrap();
    let session = StreamSession::new(
        new_session_id().unwrap(),
        Generation::default(),
        bound.authority().unwrap(),
        factory,
        None,
    )
    .unwrap();
    let mem = |bytes: Vec<u8>| Arc::new(MemoryReader::new(bytes, "scoped-port", &ledger).unwrap());
    session
        .commit(session.prepare_reader(mem(vec![1u8; 64])).unwrap())
        .unwrap();
    let sub_url = session
        .add_attachment("sub", "vtt", mem(b"WEBVTT\n\n".to_vec()))
        .unwrap();
    let playback = Arc::new(Playback {
        relay: bound.start(session.clone()).unwrap(),
        url: session.root_url().unwrap(),
        profile: None,
        kind: Kind::Video,
        title: "PortClip".into(),
        source_id: Some("src".into()),
        entry_id: Some("ep".into()),
        is_live: false,
        attachments: vec![AttachmentView {
            id: "sub".into(),
            url: sub_url,
            label: "Sub".into(),
            lang: Some("en".into()),
            default: true,
            audio: false,
        }],
        variants: Vec::new(),
        active_variant: None,
    });
    let scoped: Arc<ScopedPlayback> = playback.clone().into_scoped();
    assert_eq!(scoped.title, "PortClip");
    assert_eq!(scoped.attachments.len(), 1);
    scoped.stream.advance_seek().unwrap();
    assert_eq!(session.generation().seek, Id(1));
    scoped.stream.stop();
    wait(|| playback.relay.is_quiescent());
    drop((scoped, playback, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}
