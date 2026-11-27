use super::*;

#[test]
#[ignore = "live bilibili + libmpv end-to-end verification"]
fn live_bilibili_popular_video_plays_in_libmpv() {
    let pkg_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugin-v2/examples/community.bilibili.irisplugin");
    let bytes = std::fs::read(&pkg_path).expect("built community.bilibili.irisplugin");
    let root = TempDir::new("bili-live");
    let resources = Resources::new().unwrap();
    let package = Package::from_zip(&bytes, Provenance::User).unwrap();
    let mut installer = Installer::new(root.join("packages"), resources.ledger.clone());
    let approval = Approval {
        digest: package.digest,
        grants: package.manifest.permissions.into_iter().collect(),
        network: package.manifest.network,
        previous: None,
    };
    installer.install(&bytes, approval).unwrap();
    let hub = Hub::new(Config {
        installer,
        bootstrap: None,
        disabled: BTreeSet::new(),
        runtime: Setup {
            resources,
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        },
    })
    .unwrap();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut manager = PluginManager::new(client, None);
    wait_until(Duration::from_secs(60), "bilibili ready", || {
        manager.poll();
        manager.client().is_ready("community.bilibili")
    });
    let browse = BrowseTarget {
        source: "popular".into(),
        folder: None,
        query: None,
        filters: BTreeMap::new(),
    };
    let (_, browse_req) = manager
        .client()
        .browse("community.bilibili", browse)
        .unwrap();
    let mut chosen = Vec::new();
    wait_until(Duration::from_secs(25), "browse popular", || {
        for notice in manager.poll().notices {
            match notice {
                Notice::Page { request, page, .. } if request == browse_req => {
                    chosen = page
                        .page
                        .entries
                        .iter()
                        .filter(|e| e.kind == iris_api::SourceEntryKind::Video)
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

    let resolve_video = |manager: &mut PluginManager, id: &str, name: &str| {
        let req = manager
            .client()
            .resolve("community.bilibili", "popular", None, id, None)
            .unwrap();
        let mut resolved = None;
        wait_until(Duration::from_secs(30), "resolve video", || {
            for notice in manager.poll().notices {
                match notice {
                    Notice::Playback {
                        ticket, playback, ..
                    } if ticket.id == req => resolved = Some(Arc::new(playback)),
                    Notice::Error { code, message, .. } => {
                        panic!("resolve {id} ({name}) failed: {code:?} {message:?}")
                    }
                    _ => {}
                }
            }
            resolved.is_some()
        });
        resolved.unwrap()
    };

    let playback = resolve_video(&mut manager, &entry_id, &entry_name);
    assert!(!playback.variants.is_empty() && playback.active_variant.is_some());
    let mut player = Session::new_headless(false).unwrap();
    let mut events = player.events().unwrap();
    let open_bili = |player: &mut Session, pb: &Arc<Playback>, id: &str| {
        let uri = plugin_uri("community.bilibili", "popular", id);
        player
            .open_scoped(pb.clone().into_scoped(), uri, &Settings::default(), 0.)
            .unwrap();
    };
    open_bili(&mut player, &playback, &entry_id);

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
    let old_gen = manager.generation();
    let next_gen = Generation {
        media: Id(old_gen.media.0 + 1),
        seek: Id(0),
        context: old_gen.context,
    };
    manager.client().advance(next_gen).unwrap();
    player.release_scoped();
    let playback2 = resolve_video(&mut manager, &second_id, &second_name);
    open_bili(&mut player, &playback2, &second_id);
    wait_advance(&mut player, &mut events, "switch to video 2", 0.3, None);
    player.stop().unwrap();
    player.shutdown();
    drop((player, events, playback, playback2, manager, hub));
}
