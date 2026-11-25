use super::*;
use crate::{
    library::Kind,
    plugin_v2::{
        actor::{Executor, Operation, TaskContext},
        enhancement::{AttachmentRegistry, AttachmentSpec},
        instance::{Blob, next_id},
        ledger::Resource,
        network::NetworkGrant,
        runtime::{Resources, WindowAccess},
        tools::{ToolExecutor, ToolSetup},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    enhance::{EnhancementResource, SubtitleFormat},
    task::{ArtifactFormat, Task},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

fn wait(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(Instant::now() < deadline, "native attachment deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn native_receipts_hold_revoked_attachments_and_release_accounting() {
    let root = std::env::temp_dir().join(format!(
        "iris-enhance-{}-{}",
        std::process::id(),
        next_id().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let resources = Resources::new().unwrap();
    let ledger = resources.ledger.clone();
    let capabilities = BTreeSet::from([Capability::MediaEnhance]);
    let network = NetworkGrant::new(
        "enhance-test".into(),
        capabilities.clone(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let files = ToolExecutor::new(ToolSetup {
        network: network.clone(),
        ledger: ledger.clone(),
        capabilities: capabilities.clone(),
        temporary_directory: root.clone(),
        tools: BTreeMap::new(),
        models: BTreeMap::new(),
        catalog: Default::default(),
    })
    .unwrap();
    let access = WindowAccess {
        instance: Id(1),
        window: Id(2),
        generation: Generation::default(),
        owner: "enhance-test".into(),
        capabilities,
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
            _ => panic!("artifact worker"),
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
    let blobs = BTreeMap::from([(
        9,
        Arc::new(
            Blob::new(
                &ledger,
                &access.owner,
                b"WEBVTT\n\n00:00.000 --> 00:03.000\nReceipt\n".to_vec(),
            )
            .unwrap(),
        ),
    )]);
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
    let mut wrong = spec();
    wrong.subtitle = Some(SubtitleFormat::Ass);
    assert!(matches!(
        access.prepare_attachment(wrong),
        Err(ErrorCode::InvalidArgument)
    ));
    let mut unauthorized = access.clone();
    unauthorized.owner = "another-installation".into();
    assert!(matches!(
        unauthorized.prepare_attachment(spec()),
        Err(ErrorCode::Denied)
    ));
    drop(unauthorized);
    let track = access.prepare_attachment(spec()).unwrap();
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

    let path = root.join("video.y4m");
    std::fs::write(
        &path,
        format!(
            "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
            "FRAME\nxxxxxx".repeat(60)
        ),
    )
    .unwrap();
    let mut player = Session::new().unwrap();
    player.mpv.set_property_string("vo", "null").unwrap();
    player.mpv.set_property_string("ao", "null").unwrap();
    player.mpv.set_property_string("pause", "yes").unwrap();
    let mut events = player.events().unwrap();
    player.load(&path, Some(Kind::Video), None, 0.).unwrap();
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.state.loaded
    });
    player.attach_plugin_track(track.clone()).unwrap();
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.flush_deferred();
        !player.enhancements.tracks.active.is_empty()
    });
    let first_id = native_track(&player.mpv, track.as_ref()).expect("attached subtitle track");
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.flush_deferred();
        player
            .selected_track(super::super::TrackKind::Sub)
            .is_some_and(|sub| sub.id == first_id)
    });
    assert_eq!(
        player.mpv.get_property_string("sid").ok().as_deref(),
        Some(first_id.to_string().as_str())
    );
    // Attaching a secondary plugin subtitle must not steal selection from the first.
    let mut second_spec = spec();
    second_spec.id = "subtitle-en".into();
    second_spec.label = "English".into();
    second_spec.lang = Some("en".into());
    let second = access.prepare_attachment(second_spec).unwrap();
    player.attach_plugin_track(second.clone()).unwrap();
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.flush_deferred();
        player.enhancements.tracks.active.len() == 2
    });
    assert!(native_track(&player.mpv, second.as_ref()).is_some());
    assert_eq!(
        player
            .selected_track(super::super::TrackKind::Sub)
            .map(|sub| sub.id),
        Some(first_id)
    );
    drop(second);
    player.revoke_plugin_tracks(&access.owner);
    assert!(!player.enhancements.tracks.pending.is_empty());
    drop(track);
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.enhancements.tracks.pending.is_empty()
            && ledger.used(None, Resource::EnhancementTrack) == 0
    });

    // Revoke an addition before the asynchronous native acknowledgement arrives.
    let track = access.prepare_attachment(spec()).unwrap();
    player.attach_plugin_track(track).unwrap();
    player.revoke_plugin_tracks(&access.owner);
    assert_eq!(ledger.used(None, Resource::EnhancementTrack), 1);
    wait(|| {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        player.enhancements.tracks.pending.is_empty()
            && ledger.used(None, Resource::EnhancementTrack) == 0
    });
    player.shutdown();
    drop((player, events));
    access.cancel.cancel();
    access.files.stop();
    drop((created, written, blobs));
    wait(|| access.files.is_quiescent());
    drop(access);
    wait(|| {
        ledger.used(None, Resource::HostBytes) == 0 && ledger.used(None, Resource::TempBytes) == 0
    });
    assert!(root.starts_with(std::env::temp_dir()));
    std::fs::remove_dir_all(root).unwrap();
}
