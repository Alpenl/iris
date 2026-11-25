use super::*;
mod adaptive;
use crate::{
    library::Kind,
    player::Command,
    plugin_v2::{
        ledger::{Ledger, Resource},
        network::NetworkGrant,
        stream::{
            prepare::{AttachmentView, Playback},
            reader::MemoryReader,
            relay::BoundRelay,
            remote::RemoteFactory,
            session::{StreamSession, new_session_id},
        },
    },
};
use iris_api::v2::{Generation, NetworkDeclaration};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
fn wav() -> Vec<u8> {
    let samples = 48_000u32 * 2;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + samples * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&48000u32.to_le_bytes());
    out.extend_from_slice(&96000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(samples * 2).to_le_bytes());
    out.resize(out.len() + samples as usize * 2, 0);
    out
}
#[test]
fn actual_mpv_opens_scoped_video_audio_and_subtitles_then_restores_reference_policy() {
    let ledger = Ledger::default();
    let grant = NetworkGrant::new(
        "mpv-fixture".into(),
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
    let video = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(60)
    )
    .into_bytes();
    let reader = Arc::new(MemoryReader::new(video, "mpv-fixture", &ledger).unwrap());
    session
        .commit(session.prepare_reader(reader).unwrap())
        .unwrap();
    let audio = Arc::new(MemoryReader::new(wav(), "mpv-fixture", &ledger).unwrap());
    let audio = session.add_attachment("audio", "bin", audio).unwrap();
    let subtitle = Arc::new(
        MemoryReader::new(
            b"WEBVTT\n\n00:00.000 --> 00:02.000\nScoped subtitle\n".to_vec(),
            "mpv-fixture",
            &ledger,
        )
        .unwrap(),
    );
    let subtitle = session.add_attachment("sub", "vtt", subtitle).unwrap();
    let url = session.root_url().unwrap();
    let playback = Arc::new(Playback {
        relay: bound.start(session.clone()).unwrap(),
        url,
        profile: None,
        kind: Kind::Video,
        title: "Fixture".into(),
        is_live: false,
        attachments: vec![
            AttachmentView {
                id: "audio".into(),
                url: audio,
                label: "Audio".into(),
                lang: Some("en".into()),
                default: true,
                audio: true,
            },
            AttachmentView {
                id: "subtitle".into(),
                url: subtitle,
                label: "Subtitle".into(),
                lang: Some("en".into()),
                default: true,
                audio: false,
            },
        ],
        variants: Vec::new(),
        active_variant: None,
    });
    let mut player = Session::new().unwrap();
    player.mpv.set_property_string("vo", "null").unwrap();
    player.mpv.set_property_string("ao", "null").unwrap();
    player.mpv.set_property_string("pause", "yes").unwrap();
    let mut events = player.events().unwrap();
    player
        .open_scoped(
            playback.clone().into_scoped(),
            MediaUri::Plugin {
                plugin_id: "fixture".into(),
                source_id: "source".into(),
                entry_id: "media".into(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        let count = player
            .mpv
            .get_property_string("track-list/count")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if player.state.loaded && count >= 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "loaded={} tracks={count} format={:?}",
            player.state.loaded,
            player.mpv.get_property_string("file-format")
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        player.mpv.get_property_string("access-references").unwrap(),
        "no"
    );
    assert_eq!(player.mpv.get_property_string("demuxer").unwrap(), "lavf");
    assert_eq!(player.mpv.get_property_string("pause").unwrap(), "yes");
    assert!(
        player
            .execute(Command::SetAbLoop {
                start: f64::NAN,
                end: 1.
            })
            .is_err()
    );
    player
        .execute(Command::SetAbLoop {
            start: 0.2,
            end: 0.6,
        })
        .unwrap();
    player.execute(Command::TogglePlayback).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = 0.;
    let mut loops = 0;
    while loops < 3 {
        while let Ok(update) = events.try_recv() {
            player.apply_update(&update);
        }
        let position = player.mpv.get_property_f64("time-pos").unwrap_or(0.);
        if previous > position + 0.15 {
            loops += 1;
        }
        previous = position;
        assert!(
            Instant::now() < deadline,
            "native A-B did not repeat: loops={loops}, position={position}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.state.ab_start, Some(0.2));
    assert_eq!(player.state.ab_end, Some(0.6));
    player.execute(Command::ClearAbLoop).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while player.mpv.get_property_string("ab-loop-b").unwrap() != "no" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    player.execute(Command::SeekAbsolute(1.)).unwrap();
    assert!(session.generation().seek.0 >= 1);
    player.stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !playback.relay.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    player.shutdown();
    drop(player);
    drop(events);
    drop(playback);
    drop(session);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

#[test]
#[ignore = "live bilibili + libmpv end-to-end verification"]
fn live_bilibili_popular_video_plays_in_libmpv() {
    use crate::{
        plugin_v2::{
            hub::{Config, Hub, Notice},
            installation::{Approval, Installer},
            package::{Package, Provenance},
            requests::BrowseTarget,
            runtime::{Resources, Setup},
            ui::PluginManager,
        },
        vault::CredentialVault,
    };
    use std::collections::BTreeMap;

    let pkg_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugin-v2/examples/community.bilibili.irisplugin");
    let bytes = std::fs::read(&pkg_path).expect("built community.bilibili.irisplugin");
    let root = std::env::temp_dir().join(format!("iris-bili-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let resources = Resources::new().unwrap();
    let package = Package::from_zip(&bytes, Provenance::User).unwrap();
    let mut installer = Installer::new(root.join("packages"), resources.ledger.clone());
    installer
        .install(
            &bytes,
            Approval {
                digest: package.digest,
                grants: package.manifest.permissions.into_iter().collect(),
                network: package.manifest.network,
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
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        },
    })
    .unwrap();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut manager = PluginManager::new(client, None);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !manager.client().is_ready("community.bilibili") {
        manager.poll();
        assert!(Instant::now() < deadline, "plugin failed to become ready");
        std::thread::sleep(Duration::from_millis(20));
    }

    let (_view, browse_req) = manager
        .client()
        .browse(
            "community.bilibili",
            BrowseTarget {
                source: "popular".into(),
                folder: None,
                query: None,
                filters: BTreeMap::new(),
            },
        )
        .unwrap();
    let mut chosen_entries = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(25);
    while chosen_entries.len() < 2 {
        for notice in manager.poll().notices {
            match notice {
                Notice::Page { request, page, .. } if request == browse_req => {
                    chosen_entries = page
                        .page
                        .entries
                        .iter()
                        .filter(|e| e.kind == iris_api::SourceEntryKind::Video)
                        .take(2)
                        .map(|e| (e.id.clone(), e.name.clone()))
                        .collect();
                }
                Notice::Error { code, message, .. } => {
                    panic!("browse popular failed: {code:?} {message:?}");
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "timed out browsing popular");
        std::thread::sleep(Duration::from_millis(25));
    }
    let (entry_id, entry_name) = chosen_entries[0].clone();
    let (second_id, second_name) = chosen_entries[1].clone();
    assert!(!entry_id.is_empty() && !second_id.is_empty());

    let resolve_video = |manager: &mut PluginManager, id: &str, name: &str| {
        let req = manager
            .client()
            .resolve("community.bilibili", "popular", None, id, None)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            for notice in manager.poll().notices {
                match notice {
                    Notice::Playback {
                        ticket, playback, ..
                    } if ticket.id == req => return Arc::new(playback),
                    Notice::Error { code, message, .. } => {
                        panic!("resolve {id} ({name}) failed: {code:?} {message:?}");
                    }
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "timed out resolving {id}");
            std::thread::sleep(Duration::from_millis(25));
        }
    };

    let playback = resolve_video(&mut manager, &entry_id, &entry_name);
    assert!(
        !playback.variants.is_empty(),
        "expected stream quality variants"
    );
    assert!(playback.active_variant.is_some());

    let mut player = Session::new().unwrap();
    player.mpv.set_property_string("vo", "null").unwrap();
    player.mpv.set_property_string("ao", "null").unwrap();
    let mut events = player.events().unwrap();
    player
        .open_scoped(
            playback.clone().into_scoped(),
            MediaUri::Plugin {
                plugin_id: "community.bilibili".into(),
                source_id: "popular".into(),
                entry_id: entry_id.clone(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();

    let wait_advance = |player: &mut Session,
                        events: &mut futures::channel::mpsc::UnboundedReceiver<
        crate::player::SessionUpdate,
    >,
                        label: &str,
                        min_pos: f64,
                        max_pos: Option<f64>| {
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut first_in_range: Option<f64> = None;
        loop {
            while let Ok(update) = events.try_recv() {
                if let Some(crate::player::SessionEvent::Error(code)) = player.apply_update(&update)
                {
                    panic!("libmpv playback error {code} during {label}");
                }
            }
            player.flush_deferred();
            let tracks = player
                .mpv
                .get_property_string("track-list/count")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let aid = player.mpv.get_property_string("aid").unwrap_or_default();
            let pos = player.mpv.get_property_f64("time-pos").unwrap_or(0.);
            let in_range = pos >= min_pos && max_pos.is_none_or(|hi| pos <= hi);
            if player.state.loaded && tracks >= 2 && aid != "no" && in_range {
                let start = *first_in_range.get_or_insert(pos);
                if pos >= start + 0.6 {
                    return pos;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{label} did not advance: loaded={} tracks={} aid={aid:?} pos={pos} dur={}",
                player.state.loaded,
                tracks,
                player.state.duration
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };

    wait_advance(&mut player, &mut events, "initial play", 0.3, None);
    let dur = player.state.duration;
    assert!(dur > 5.0, "expected video duration > 5s, got {dur}");

    // Seek forward past 60% and verify video + audio advance smoothly.
    let fwd = (dur * 0.65).clamp(2.0, dur - 2.0);
    player.execute(Command::SeekAbsolute(fwd)).unwrap();
    wait_advance(&mut player, &mut events, "forward seek", fwd - 3.0, None);

    // Seek backward to 25% and verify video + audio still advance smoothly.
    let back = (dur * 0.25).clamp(1.0, fwd - 1.5);
    player.execute(Command::SeekAbsolute(back)).unwrap();
    wait_advance(
        &mut player,
        &mut events,
        "backward seek",
        (back - 3.0).max(0.0),
        Some(fwd - 0.5),
    );

    // Switch to a second Bilibili video during active playback.
    let old_gen = manager.generation();
    manager
        .client()
        .advance(Generation {
            media: iris_api::v2::Id(old_gen.media.0 + 1),
            seek: iris_api::v2::Id(0),
            context: old_gen.context,
        })
        .unwrap();
    player.release_scoped();
    let playback2 = resolve_video(&mut manager, &second_id, &second_name);
    player
        .open_scoped(
            playback2.clone().into_scoped(),
            MediaUri::Plugin {
                plugin_id: "community.bilibili".into(),
                source_id: "popular".into(),
                entry_id: second_id.clone(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();
    wait_advance(&mut player, &mut events, "switch to video 2", 0.3, None);

    player.stop().unwrap();
    player.shutdown();
    drop(player);
    drop(events);
    drop((playback, playback2));
    drop((manager, hub));
    let _ = std::fs::remove_dir_all(root);
}
