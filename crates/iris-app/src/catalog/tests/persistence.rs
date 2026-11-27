use std::time::Duration;

use iris_core::bookmarks::{LocationStore, PinnedBookmark};
use iris_core::catalog::LocalDirectory;
use iris_core::media_uri::MediaUri;
use iris_core::store::Store;

use super::CatalogRig;
use crate::catalog::{BookmarkAction, SAVE_EVERY};
use crate::plugins::PlayerProgress;

fn hex_ascii(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn bookmarks_persistence_and_background_webdav_migration_sync() {
    let mut rig = CatalogRig::new("catalog-bookmarks");
    let movies_dir = rig.dir.0.join("Movies");
    std::fs::create_dir_all(&movies_dir).unwrap();

    assert!(rig.catalog.pin_current_location().is_err());

    rig.catalog.set_local_directory(
        LocalDirectory {
            folder: Some(movies_dir.clone()),
            entries: Vec::new(),
            subfolders: Vec::new(),
        },
        &rig.plugins,
        None,
        None,
    );
    let bm1 = rig.catalog.pin_current_location().expect("pin local dir");
    assert_eq!(bm1.id, "bm-1");
    assert_eq!(bm1.name, "Movies");
    assert_eq!(bm1.uri, MediaUri::Local(movies_dir.clone()));

    let bm2 = rig.catalog.pin_bookmark(
        "Live Stream".into(),
        MediaUri::HttpUrl("https://live.example.com/ch1.m3u8".into()),
        None,
    );
    assert_eq!(bm2.id, "bm-2");

    let bm3 = rig.catalog.pin_bookmark(
        "NAS Anime".into(),
        MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: "legacy-slot".into(),
            entry_id: "/Anime".into(),
        },
        Some("/Anime".into()),
    );
    assert_eq!(bm3.id, "bm-3");

    assert!(rig.catalog.remove_bookmark("bm-2"));
    let bm2_reused = rig.catalog.pin_bookmark(
        "Backup Stream".into(),
        MediaUri::HttpUrl("https://live.example.com/ch2.m3u8".into()),
        None,
    );
    assert_eq!(bm2_reused.id, "bm-2");

    let disk_locations = LocationStore::load_from(&rig.locations_file());
    assert_eq!(disk_locations.bookmarks.len(), 3);

    assert_eq!(
        rig.catalog.prepare_open_bookmark(&bm1),
        BookmarkAction::OpenLocal(movies_dir)
    );
    assert_eq!(
        rig.catalog.prepare_open_bookmark(&bm3),
        BookmarkAction::BrowsePlugin {
            plugin_id: "iris.webdav".into(),
            source_id: "legacy-slot".into(),
            folder_id: Some("/Anime".into()),
        }
    );
    assert_eq!(
        rig.catalog.prepare_open_bookmark(&bm2_reused),
        BookmarkAction::OpenDirectStream {
            url: "https://live.example.com/ch2.m3u8".into(),
            title: Some("Backup Stream".into()),
        }
    );

    // Simulate background `iris.webdav` source migration rewriting `legacy-slot` -> `dav-1` in `locations.json`.
    let mut migrated = LocationStore::load_from(&rig.locations_file());
    migrated.add_bookmark(PinnedBookmark {
        id: "bm-3".into(),
        name: "NAS Anime".into(),
        uri: MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: "dav-1".into(),
            entry_id: "/Anime".into(),
        },
        folder_id: Some("/Anime".into()),
    });
    rig.catalog.apply_plugin_domain_changes(
        crate::PluginDomainChanges {
            locations_reloaded: true,
            ..Default::default()
        },
        &rig.plugins,
        None,
        None,
    );
    let synced_bm3 = rig
        .catalog
        .bookmarks()
        .iter()
        .find(|b| b.id == "bm-3")
        .expect("bm-3 preserved");
    assert_eq!(
        synced_bm3.uri,
        MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: "dav-1".into(),
            entry_id: "/Anime".into(),
        }
    );
}

#[test]
fn store_flush_intent_receipt_continuation_write_failure_retry_privacy_and_shutdown() {
    let mut rig = CatalogRig::new("catalog-persistence");
    let ep1 = rig.dir.0.join("ep1.mkv");
    let ep2 = rig.dir.0.join("ep2.mkv");
    std::fs::write(&ep1, b"1").unwrap();
    std::fs::write(&ep2, b"2").unwrap();

    let t0 = rig.catalog.last_save();
    let prog1 = Some(PlayerProgress::Local {
        path: ep1.clone(),
        pos: 42.0,
        dur: 120.0,
        eof: false,
    });

    // 1. `< SAVE_EVERY` does not flush; `>= SAVE_EVERY` records progress and emits `StoreFlushIntent`.
    assert!(
        rig.catalog
            .maybe_periodic_flush(
                prog1.clone(),
                true,
                false,
                t0 + SAVE_EVERY - Duration::from_millis(1)
            )
            .is_none()
    );
    let intent_1 = rig
        .catalog
        .maybe_periodic_flush(prog1, true, false, t0 + SAVE_EVERY)
        .expect("periodic flush intent emitted");
    assert!(rig.catalog.is_store_flush_in_flight());

    // 2. Concurrent mutation during `intent_1` marks `mutated_during_store_flush` and chains `intent_2` on receipt!
    assert!(rig.catalog.record_progress(
        Some(PlayerProgress::Local {
            path: ep2.clone(),
            pos: 30.0,
            dur: 100.0,
            eof: false,
        }),
        true,
        false,
    ));
    assert!(rig.catalog.mutated_during_store_flush());
    assert!(
        rig.catalog.begin_store_flush().is_none(),
        "must not spawn overlapping background flush while intent_1 is in flight"
    );

    let receipt_1 = intent_1.run();
    assert!(receipt_1.succeeded());
    let intent_2 = rig
        .catalog
        .complete_store_flush(receipt_1)
        .expect("concurrent mutation must chain follow-up StoreFlushIntent");
    assert!(rig.catalog.is_store_flush_in_flight());
    let receipt_2 = intent_2.run();
    assert!(receipt_2.succeeded());
    assert!(rig.catalog.complete_store_flush(receipt_2).is_none());
    assert!(!rig.catalog.is_store_flush_in_flight());
    assert!(!rig.catalog.store().is_dirty());

    let reloaded = Store::load_from_dir(&rig.data_dir());
    assert!((reloaded.resume_path(&ep1) - 42.0).abs() < 0.01);
    assert!((reloaded.resume_path(&ep2) - 30.0).abs() < 0.01);

    // 3. Stale receipt superseded by `save_progress_sync` + newer async flush leaves newer flush intact!
    assert!(rig.catalog.record_progress(
        Some(PlayerProgress::Local {
            path: ep1.clone(),
            pos: 55.0,
            dur: 120.0,
            eof: false,
        }),
        true,
        false,
    ));
    let stale_intent = rig.catalog.begin_store_flush().unwrap();
    rig.catalog.save_progress_sync(
        Some(PlayerProgress::Local {
            path: ep1.clone(),
            pos: 60.0,
            dur: 120.0,
            eof: false,
        }),
        true,
        false,
        t0 + Duration::from_secs(12),
    );
    assert!(!rig.catalog.is_store_flush_in_flight());

    assert!(rig.catalog.record_progress(
        Some(PlayerProgress::Local {
            path: ep1.clone(),
            pos: 75.0,
            dur: 120.0,
            eof: false,
        }),
        true,
        false,
    ));
    let active_intent = rig.catalog.begin_store_flush().unwrap();
    assert!(
        rig.catalog
            .complete_store_flush(stale_intent.run())
            .is_none()
    );
    assert!(
        rig.catalog.is_store_flush_in_flight(),
        "stale receipt must leave active flush intact"
    );
    assert!(
        rig.catalog
            .complete_store_flush(active_intent.run())
            .is_none()
    );
    assert!(!rig.catalog.is_store_flush_in_flight());
    assert!(!rig.catalog.store().is_dirty());

    // 4. Cross-platform disk write failure (`records.lock` blocked as a directory) retains dirty bits
    //    without spin-retrying, then succeeds cleanly on `flush_on_shutdown` once unblocked!
    let lock_path = rig.data_dir().join("records.lock");
    let _ = std::fs::remove_file(&lock_path);
    std::fs::create_dir(&lock_path).expect("inject cross-platform lock open failure");

    assert!(rig.catalog.record_progress(
        Some(PlayerProgress::Local {
            path: ep1.clone(),
            pos: 95.0,
            dur: 120.0,
            eof: false,
        }),
        true,
        false,
    ));
    let fail_intent = rig.catalog.begin_store_flush().unwrap();
    let fail_receipt = fail_intent.run();
    assert!(!fail_receipt.succeeded());
    assert!(
        rig.catalog.complete_store_flush(fail_receipt).is_none(),
        "failed flush must not spin-loop immediately"
    );
    assert!(
        rig.catalog.store().is_dirty(),
        "failed flush must retain dirty bits for subsequent retry/shutdown"
    );

    let fav_ep2 = MediaUri::Local(ep2.clone());
    assert!(
        rig.catalog
            .toggle_favorite(&fav_ep2, &rig.plugins, None, None)
    );
    rig.catalog
        .save_progress_sync(None, true, false, t0 + Duration::from_secs(20));
    assert!(
        rig.catalog.store().is_dirty(),
        "sync save failure must also retain dirty bits"
    );

    std::fs::remove_dir(&lock_path).unwrap();

    // 5. Private-identity & tokenized query-URL suppression + synchronous shutdown flush!
    let secret_http =
        MediaUri::HttpUrl("https://cdn.example.com/stream.m3u8?token=topsecret999".into());
    let private_plugin = MediaUri::Plugin {
        plugin_id: "demo.priv".into(),
        source_id: "s".into(),
        entry_id: "secret-ep".into(),
    };
    let public_http = MediaUri::HttpUrl("https://cdn.example.com/public_talk.mp4".into());
    let placeholder_plugin = MediaUri::Plugin {
        plugin_id: "demo.pub".into(),
        source_id: "s".into(),
        entry_id: "clip-1".into(),
    };

    rig.catalog.record_media_opened(
        Some(secret_http.clone()),
        secret_http.may_share_identity(),
        Some("Secret Token Stream"),
    );
    rig.catalog
        .record_media_opened(Some(private_plugin.clone()), false, Some("Private Stream"));
    rig.catalog
        .record_media_opened(Some(public_http.clone()), true, Some("Public Talk"));
    rig.catalog
        .record_media_opened(Some(placeholder_plugin.clone()), true, Some("插件媒体"));
    assert!(!rig.catalog.record_progress(
        Some(PlayerProgress::Uri {
            uri: secret_http.to_uri_string(),
            pos: 50.0,
            dur: 200.0,
            eof: false,
        }),
        true,
        false,
    ));

    rig.catalog.flush_on_shutdown(
        Some(PlayerProgress::Uri {
            uri: public_http.to_uri_string(),
            pos: 88.0,
            dur: 200.0,
            eof: false,
        }),
        true,
        false,
        t0 + Duration::from_secs(25),
    );
    assert!(!rig.catalog.store().is_dirty());

    let records_raw = std::fs::read_to_string(rig.data_dir().join("records.tsv")).unwrap();
    let recent_raw = std::fs::read_to_string(rig.data_dir().join("recent.tsv")).unwrap();
    let secret_hex = hex_ascii("topsecret999");
    let private_hex = hex_ascii("secret-ep");
    assert!(!records_raw.contains(&secret_hex));
    assert!(!recent_raw.contains(&secret_hex));
    assert!(!recent_raw.contains(&private_hex));
    assert!(recent_raw.contains(&hex_ascii("https://cdn.example.com/public_talk.mp4")));
    assert!(recent_raw.contains(&hex_ascii("Public Talk")));

    let final_store = Store::load_from_dir(&rig.data_dir());
    assert!(final_store.is_favorite(&fav_ep2));
    assert!((final_store.resume_path(&ep1) - 95.0).abs() < 0.01);
    assert!((final_store.resume(&public_http) - 88.0).abs() < 0.01);
    assert!(final_store.recent().contains(&public_http));
    assert_eq!(final_store.recent_title(&public_http), Some("Public Talk"));
    assert_eq!(final_store.recent_title(&placeholder_plugin), None);
    assert!(!final_store.recent().contains(&secret_http));
    assert!(!final_store.recent().contains(&private_plugin));
}
