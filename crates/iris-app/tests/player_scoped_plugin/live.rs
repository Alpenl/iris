use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use iris_api::SourceEntryKind;
use iris_core::{
    player::{Command, ScopedPlayback, Session, SessionEvent, SessionUpdate},
    settings::Settings,
    vault::CredentialVault,
};
use iris_plugin_host::{BrowseTarget, Notice, PluginHost, WindowSession};

use super::{TempDir, plugin_uri, wait_until};

#[test]
#[ignore = "live bilibili + libmpv end-to-end verification"]
fn live_bilibili_popular_video_plays_in_libmpv() {
    let pkg_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugin-v2/examples/community.bilibili.irisplugin");
    assert!(
        pkg_path.is_file(),
        "expected built community.bilibili.irisplugin at {}",
        pkg_path.display()
    );
    let root = TempDir::new("bili-live");
    let host = PluginHost::open_in(&root, CredentialVault::in_memory(), &[], false).unwrap();
    let (mut manager, _wake) = host.open_window(&[], "graphite");
    manager.review_package(pkg_path).unwrap();

    let mut review_id = None;
    let mut grants = BTreeSet::new();
    wait_until(Duration::from_secs(20), "bilibili review", || {
        for notice in manager.poll().notices {
            if let Notice::Review(summary) = notice {
                review_id = Some(summary.id);
                grants = summary.manifest.permissions.iter().copied().collect();
            }
        }
        review_id.is_some()
    });
    manager
        .approve_review(review_id.unwrap(), grants, false)
        .unwrap();

    wait_until(Duration::from_secs(60), "bilibili ready", || {
        let live_ready = manager.is_ready("community.bilibili");
        let _ = manager.poll();
        live_ready
            && manager.list().iter().any(|p| {
                p.id == "community.bilibili" && p.status == iris_plugin_host::PluginStatus::Ready
            })
    });

    let browse = BrowseTarget {
        source: "popular".into(),
        folder: None,
        query: None,
        filters: BTreeMap::new(),
    };
    let (_, browse_req) = manager.browse("community.bilibili", browse).unwrap();
    let mut chosen = Vec::new();
    wait_until(Duration::from_secs(25), "browse popular", || {
        for notice in manager.poll().notices {
            match notice {
                Notice::Page { request, page, .. } if request == browse_req => {
                    chosen = page
                        .page
                        .entries
                        .iter()
                        .filter(|e| e.kind == SourceEntryKind::Video)
                        .take(2)
                        .map(|e| (e.id.clone(), e.name.clone()))
                        .collect();
                }
                Notice::Error { code, message, .. } => {
                    panic!("browse popular failed: {code:?} {message:?}")
                }
                _ => {}
            }
        }
        chosen.len() >= 2
    });
    let ((entry_id, entry_name), (second_id, second_name)) = (chosen[0].clone(), chosen[1].clone());
    assert!(!entry_id.is_empty() && !second_id.is_empty());

    let resolve_and_open = |manager: &mut WindowSession,
                            player: &mut Session,
                            id: &str,
                            name: &str|
     -> Arc<ScopedPlayback> {
        let req = manager
            .resolve("community.bilibili", "popular", None, id, None)
            .unwrap();
        let mut opened = None;
        wait_until(Duration::from_secs(30), "resolve video", || {
            for notice in manager.poll().notices {
                match notice {
                    Notice::Playback {
                        generation,
                        ticket,
                        playback,
                        ..
                    } if ticket.id == req => {
                        assert!(!playback.variants.is_empty() && playback.active_variant.is_some());
                        let uri = plugin_uri("community.bilibili", "popular", id);
                        manager
                            .admit_playback("community.bilibili", generation, playback, |scoped| {
                                opened = Some(scoped.clone());
                                player.open_scoped(scoped, uri, &Settings::default(), 0.)
                            })
                            .unwrap()
                            .unwrap();
                    }
                    Notice::Error { code, message, .. } => {
                        panic!("resolve {id} ({name}) failed: {code:?} {message:?}")
                    }
                    _ => {}
                }
            }
            opened.is_some()
        });
        opened.unwrap()
    };

    let mut player = Session::new_headless(false).unwrap();
    let mut events = player.events().unwrap();
    let scoped1 = resolve_and_open(&mut manager, &mut player, &entry_id, &entry_name);

    let wait_advance = |player: &mut Session,
                        events: &mut UnboundedReceiver<SessionUpdate>,
                        label: &str,
                        min_pos: f64,
                        max_pos: Option<f64>| {
        let mut first_in_range: Option<f64> = None;
        wait_until(Duration::from_secs(25), label, || {
            while let Ok(update) = events.try_recv() {
                if let Some(SessionEvent::Error(code)) = player.apply_update(&update) {
                    panic!("libmpv playback error {code} during {label}");
                }
            }
            player.flush_deferred();
            let pos = player.state().time_pos;
            let in_range = pos >= min_pos && max_pos.is_none_or(|hi| pos <= hi);
            player.state().loaded
                && player.backend_diagnostics().track_count >= 2
                && player.state().audio != "no"
                && in_range
                && pos >= *first_in_range.get_or_insert(pos) + 0.6
        });
    };

    wait_advance(&mut player, &mut events, "initial play", 0.3, None);
    let dur = player.state().duration;
    assert!(dur > 5.0, "expected video duration > 5s, got {dur}");
    let fwd = (dur * 0.65).clamp(2.0, dur - 2.0);
    player.execute(Command::SeekAbsolute(fwd)).unwrap();
    wait_advance(&mut player, &mut events, "forward seek", fwd - 3.0, None);
    let back = (dur * 0.25).clamp(1.0, fwd - 1.5);
    player.execute(Command::SeekAbsolute(back)).unwrap();
    wait_advance(
        &mut player,
        &mut events,
        "backward seek",
        (back - 3.0).max(0.0),
        Some(fwd - 0.5),
    );
    manager.advance_media().unwrap();
    player.release_scoped();
    let scoped2 = resolve_and_open(&mut manager, &mut player, &second_id, &second_name);
    wait_advance(&mut player, &mut events, "switch to video 2", 0.3, None);
    player.stop().unwrap();
    player.release_scoped();
    player.shutdown();
    manager.shutdown();
    host.shutdown();
    drop((player, events, scoped1, scoped2, manager, host));
}
