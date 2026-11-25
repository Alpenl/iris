use super::recent::MAX_RECENT;
use super::*;

fn hex_str(s: &str) -> String {
    let mut out = String::new();
    push_hex(s.as_bytes(), &mut out);
    out
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("iris-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn scrub_current_files_even_past_the_recent_limit() {
    let dir = test_dir("wide-scrub");
    let wide = "https://cdn.test/video?ticket=secret"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let mut key = String::new();
    push_hex(&wide, &mut key);
    let mut recent = String::new();
    for i in 0..MAX_RECENT {
        push_hex(format!("file-{i}.mkv").as_bytes(), &mut recent);
        recent.push('\n');
    }
    recent.push_str(&key);
    std::fs::write(dir.join("recent.tsv"), recent).unwrap();
    std::fs::write(dir.join("records.tsv"), format!("{key}\t1\t10\t20\n")).unwrap();
    let store = Store::load_from_dir(&dir);
    assert_eq!(store.recent().len(), MAX_RECENT);
    assert!(store.records().is_empty());
    assert!(
        !std::fs::read_to_string(dir.join("recent.tsv"))
            .unwrap()
            .contains(&key)
    );
    assert!(std::fs::read(dir.join("records.tsv")).unwrap().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn session_only_links_never_become_records_or_history() {
    let dir = test_dir("privacy");
    let mut store = Store::load_from_dir(&dir);
    for raw in [
        "https://user:password@example.test/clip.mp4",
        "https://bearer@example.test/clip.mp4",
        "https://example.test/clip.mp4?provider-specific-field=private",
        "https://example.test/clip.mp4#access=private",
    ] {
        let uri = MediaUri::parse(raw);
        store.push_recent(&uri);
        assert!(!store.remember(&uri, 40., 120., false));
        assert!(!store.toggle_favorite(&uri));
    }
    assert!(store.recent().is_empty());
    assert!(store.records().is_empty());
    assert!(!store.is_dirty());
    assert!(!dir.join("records.tsv").exists());
    assert!(!dir.join("recent.tsv").exists());
    let public = MediaUri::parse("https://example.test/public/clip.mp4");
    store.push_recent(&public);
    assert!(store.remember(&public, 40., 120., false));
    assert!(store.toggle_favorite(&public));
    let reopened = Store::load_from_dir(&dir);
    assert_eq!(reopened.recent(), std::slice::from_ref(&public));
    assert_eq!(reopened.resume(&public), 40.);
    assert!(reopened.is_favorite(&public));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn scrub_existing_files_and_retire_both_legacy_lists_before_clear() {
    let dir = test_dir("scrub");
    let unsafe_keys = [
        hex_str("https://a.test/1?ticket=private"),
        hex_str("https://secret@a.test/2"),
    ];
    let plugin = MediaUri::parse("iris-plugin://provider/source/item");
    let stable = hex_str(&plugin.to_uri_string());
    for (records, recent, key) in [
        ("records.tsv", "recent.tsv", &unsafe_keys[0]),
        ("uri_records.tsv", "recent_uris.tsv", &unsafe_keys[1]),
    ] {
        std::fs::write(
            dir.join(records),
            format!("{stable}\t1\t30\t90\n{key}\t1\t20\t100\n"),
        )
        .unwrap();
        std::fs::write(dir.join(recent), format!("{stable}\n{key}\n")).unwrap();
    }
    let mut store = Store::load_from_dir(&dir);
    assert_eq!(store.records().len(), 1);
    assert_eq!(store.recent(), std::slice::from_ref(&plugin));
    assert!(store.is_favorite(&plugin));
    for filename in ["records.tsv", "recent.tsv"] {
        let contents = std::fs::read_to_string(dir.join(filename)).unwrap();
        assert!(contents.contains(&stable));
        assert!(
            unsafe_keys
                .iter()
                .all(|key| !contents.contains(key.as_str()))
        );
    }
    assert!(!dir.join("uri_records.tsv").exists());
    assert!(!dir.join("recent_uris.tsv").exists());
    store.clear_recent();
    assert!(store.remove_favorite(&plugin));
    assert!(store.reset_progress(&plugin));
    let reopened = Store::load_from_dir(&dir);
    assert!(reopened.records().is_empty());
    assert!(reopened.recent().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_flush_retains_edits_and_preserves_previous_data() {
    let dir = test_dir("retry");
    let first = MediaUri::parse("first.mkv");
    let second = MediaUri::parse("second.mkv");
    let mut store = Store::load_from_dir(&dir);
    assert!(store.toggle_favorite(&first));
    store.push_recent(&first);
    // Make the existing files unreadable as TSV without permission tricks or sleeps.
    for file in ["records", "recent"] {
        let path = dir.join(format!("{file}.tsv"));
        std::fs::rename(&path, dir.join(file)).unwrap();
        std::fs::create_dir(path).unwrap();
    }
    assert!(store.remember(&second, 30., 90., false));
    store.push_recent(&second);
    assert!(store.is_dirty());
    assert!(store.remember(&second, 45., 90., false));
    for file in ["records", "recent"] {
        let path = dir.join(format!("{file}.tsv"));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(dir.join(file), path).unwrap();
    }
    store.save();
    assert!(!store.is_dirty());
    let reopened = Store::load_from_dir(&dir);
    assert!(reopened.is_favorite(&first));
    assert_eq!(reopened.resume(&second), 45.);
    assert_eq!(reopened.recent(), &[second, first]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_instances_merge_fields_and_never_restore_unrelated_deletes() {
    let dir = test_dir("merge");
    let x = MediaUri::parse("x.mkv");
    let y = MediaUri::parse("y.mkv");
    let mut a = Store::load_from_dir(&dir);
    let mut b = Store::load_from_dir(&dir);
    assert!(a.remember(&x, 30., 100., false));
    assert!(b.toggle_favorite(&x));
    a.save();
    assert!(a.is_favorite(&x));
    assert!(b.toggle_favorite(&y));
    assert_eq!(b.resume(&x), 30.);
    a.push_recent(&x);
    b.push_recent(&y);
    assert_eq!(b.recent(), &[y.clone(), x.clone()]);
    b.clear_recent();
    a.push_recent(&y);
    assert!(b.reset_progress(&x));
    assert!(b.remove_favorite(&x));
    assert!(a.remember(&y, 50., 100., false));
    a.save();
    let reopened = Store::load_from_dir(&dir);
    assert!(!reopened.records().contains_key(&x.to_uri_string()));
    assert_eq!(reopened.recent(), std::slice::from_ref(&y));
    assert!(reopened.is_favorite(&y));
    assert_eq!(reopened.resume(&y), 50.);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn progress_pending_before_another_instances_reset_does_not_undo_it() {
    let dir = test_dir("pending-reset");
    let x = MediaUri::parse("episode.mkv");
    let mut first = Store::load_from_dir(&dir);
    assert!(first.remember(&x, 30., 100., false));
    first.save();
    let mut second = Store::load_from_dir(&dir);
    assert!(first.remember(&x, 35., 100., false));
    assert!(second.reset_progress(&x));
    first.save();
    assert_eq!(first.resume(&x), 0.);
    assert!(Store::load_from_dir(&dir).records().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unified_store_roundtrips_and_migrates_legacy_path_and_webdav_tsvs() {
    let dir = std::env::temp_dir().join(format!("iris-store-unified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Seed legacy `records.tsv` (raw local path) + legacy `uri_records.tsv` (`webdav://` URI)
    // + legacy `recent.tsv` + `recent_uris.tsv` + corrupt row.
    let legacy_local = "/素材/hello\tworld\n100%.mp4";
    let legacy_webdav = "webdav://home-nas/Movies/Sci%20Fi/ep1.mkv";
    std::fs::write(
        dir.join("records.tsv"),
        format!(
            "{}\t1\t25.000\t100.000\ninvalid\t1\tNaN\t60\n",
            hex_str(legacy_local)
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("uri_records.tsv"),
        format!("{}\t1\t30.000\t120.000\n", hex_str(legacy_webdav)),
    )
    .unwrap();
    std::fs::write(
        dir.join("recent.tsv"),
        format!("{}\n", hex_str(legacy_local)),
    )
    .unwrap();
    std::fs::write(
        dir.join("recent_uris.tsv"),
        format!("{}\n", hex_str(legacy_webdav)),
    )
    .unwrap();

    let mut store = Store::load_from_dir(&dir);
    assert!(store.is_favorite_path(Path::new(legacy_local)));
    assert_eq!(store.resume_path(Path::new(legacy_local)), 25.0);

    let upgraded_webdav = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: "home-nas".into(),
        entry_id: "/Movies/Sci Fi/ep1.mkv".into(),
    };
    assert!(store.is_favorite(&upgraded_webdav));
    assert_eq!(store.progress_ratio_uri(&upgraded_webdav), Some(0.25));
    assert_eq!(store.recent().len(), 2);

    // A synchronous flush acknowledges changes only after the write succeeds.
    let cloud_item = MediaUri::Plugin {
        plugin_id: "acme.cloud".into(),
        source_id: "drive".into(),
        entry_id: "ep2".into(),
    };
    assert!(store.remember(&cloud_item, 15.0, 60.0, false));
    assert!(store.is_dirty());
    store.save();
    assert!(!store.is_dirty());

    assert!(!dir.join("uri_records.tsv").exists());
    assert!(!dir.join("recent_uris.tsv").exists());
    let mut reloaded = Store::load_from_dir(&dir);
    assert!(reloaded.is_favorite(&upgraded_webdav));
    assert_eq!(reloaded.resume(&cloud_item), 15.0);

    // EOF prunes non-favorite progress while keeping favorites.
    assert!(reloaded.remember(&cloud_item, 60.0, 60.0, true));
    assert_eq!(reloaded.clear_progress(), 2);
    assert!(reloaded.is_favorite(&upgraded_webdav));
    assert_eq!(reloaded.resume(&upgraded_webdav), 0.0);
    assert_eq!(reloaded.resume(&cloud_item), 0.0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_pruning_retains_favorites_and_just_edited_progress() {
    let mut records = Records::new();
    let progress = Record {
        position: 10.,
        duration: 100.,
        ..Record::default()
    };
    for index in 0..MAX_RECORDS {
        records.insert(format!("clip-{index:04}.mp4"), progress);
    }
    records.insert(
        "a-favorite.mp4".into(),
        Record {
            favorite: true,
            ..Record::default()
        },
    );
    records.insert("a-new.mp4".into(), progress);
    let edits = BTreeMap::from([("a-new.mp4".into(), RecordEdit::default())]);
    assert!(prune_records(&mut records, &edits));
    assert_eq!(records.len(), MAX_RECORDS);
    assert!(records.contains_key("a-favorite.mp4"));
    assert!(records.contains_key("a-new.mp4"));
}

#[test]
fn locked_records_keep_pending_edits_for_the_next_save() {
    let dir = test_dir("records-lock");
    let clip = MediaUri::parse("new.mp4");
    let mut store = Store::load_from_dir(&dir);
    let lock = fs_util::lock_file(&dir.join("records.lock")).unwrap();
    assert!(store.toggle_favorite(&clip));
    assert!(store.is_dirty());
    assert!(!dir.join("records.tsv").exists());
    drop(lock);
    store.save();
    assert!(!store.is_dirty());
    assert!(Store::load_from_dir(&dir).is_favorite(&clip));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn opening_while_locked_loads_history_and_preserves_the_resume_baseline() {
    let dir = test_dir("startup-lock");
    let clip = MediaUri::parse("resume.mkv");
    let mut writer = Store::load_from_dir(&dir);
    assert!(writer.toggle_favorite(&clip));
    assert!(writer.remember(&clip, 30., 100., false));
    writer.push_recent(&clip);

    let lock = fs_util::lock_file(&dir.join("records.lock")).unwrap();
    let mut reader = Store::load_from_dir(&dir);
    assert_eq!(reader.recent(), std::slice::from_ref(&clip));
    assert!(reader.is_favorite(&clip));
    assert_eq!(reader.resume(&clip), 30.);
    assert!(reader.remember(&clip, 40., 100., false));
    drop(lock);

    reader.save();
    assert_eq!(Store::load_from_dir(&dir).resume(&clip), 40.);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_legacy_unc_record_migrates_to_local_path_encoding() {
    let dir = test_dir("legacy-unc");
    let raw = hex_str("file://nas/media/clip.mp4");
    let path = dir.join("records.tsv");
    std::fs::write(&path, format!("{raw}\t1\t10\t20\n")).unwrap();
    let expected = MediaUri::from(Path::new(r"\\nas\media\clip.mp4"));
    let store = Store::load_from_dir(&dir);
    assert!(store.is_favorite(&expected));
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .starts_with(&hex_str(&expected.to_uri_string()))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recent_titles_persist_sanitize_preserve_and_backfill_legacy_entries() {
    let dir = test_dir("recent-titles");
    let bv1 = MediaUri::Plugin {
        plugin_id: "community.bilibili".into(),
        source_id: "recommend".into(),
        entry_id: "BV1nqyBzqEgd".into(),
    };
    let bv2 = MediaUri::Plugin {
        plugin_id: "community.bilibili".into(),
        source_id: "popular".into(),
        entry_id: "BV1EmykzjEU3".into(),
    };

    // Seed legacy 1-column `recent.tsv` without titles (exact format from older Iris versions).
    std::fs::write(
        dir.join("recent.tsv"),
        format!(
            "{}\n{}\n",
            hex_str(&bv1.to_uri_string()),
            hex_str(&bv2.to_uri_string())
        ),
    )
    .unwrap();

    let mut store = Store::load_from_dir(&dir);
    assert_eq!(store.recent(), &[bv1.clone(), bv2.clone()]);
    assert_eq!(store.recent_title(&bv1), None);
    assert_eq!(store.recent_display_name(&bv1), "BV1nqyBzqEgd");

    // Backfilling from a catalog page (even another source like "history") populates titles
    // without reordering recency.
    assert!(store.backfill_plugin_recent_titles(
        "community.bilibili",
        "history",
        [("BV1EmykzjEU3", "  【4K】实拍\t演示\u{202E}\n视频  ")],
    ));
    assert_eq!(store.recent(), &[bv1.clone(), bv2.clone()]);
    assert_eq!(store.recent_display_name(&bv2), "【4K】实拍 演示 视频");

    // Opening BV1 records its title even when it is already at index 0.
    store.push_recent_with_title(&bv1, Some("第一个测试标题"));
    assert_eq!(store.recent_display_name(&bv1), "第一个测试标题");

    // Re-pushing without a title preserves the previously known title.
    store.push_recent(&bv1);
    assert_eq!(store.recent_display_name(&bv1), "第一个测试标题");

    // Reloading from disk round-trips both titles cleanly.
    let mut reopened = Store::load_from_dir(&dir);
    assert_eq!(reopened.recent(), &[bv1.clone(), bv2.clone()]);
    assert_eq!(reopened.recent_display_name(&bv1), "第一个测试标题");
    assert_eq!(reopened.recent_display_name(&bv2), "【4K】实拍 演示 视频");

    // Clearing recent clears both URIs and titles.
    reopened.clear_recent();
    assert!(reopened.recent().is_empty());
    assert_eq!(reopened.recent_title(&bv1), None);
    std::fs::remove_dir_all(dir).unwrap();
}
