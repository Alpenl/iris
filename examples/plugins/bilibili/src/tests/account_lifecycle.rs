use super::*;

const NOW: u64 = 1_700_000_000_000;

fn action(id: &str) -> HostEvent<Value> {
    HostEvent::Action {
        id: id.into(),
        value: None,
        snapshot: None,
    }
}

fn logout(plugin: &mut BilibiliPlugin, host: &mut MockHost, window: u64) {
    plugin
        .event(
            HostEvent::Window {
                window: Id(window),
                generation: Generation::default(),
                body: Box::new(action("logout")),
            },
            host,
            NOW,
        )
        .unwrap();
    assert!(!plugin.session.is_authenticated());
    assert!(matches!(
        host.pop().1,
        Task::SecretWrite { value: None, .. }
    ));
}

fn cookie_login(plugin: &mut BilibiliPlugin, host: &mut MockHost, cookie: &str) -> Id {
    plugin
        .event(
            win(HostEvent::ModalSubmit {
                id: "bili.login".into(),
                values: BTreeMap::from([("cookie".into(), cookie.into())]),
            }),
            host,
            NOW,
        )
        .unwrap();
    let (op, task) = host.pop();
    assert!(matches!(task, Task::SecretWrite { value: Some(_), .. }));
    op
}

#[test]
fn logout_revokes_pending_secret_write_and_allows_a_fresh_login() {
    for window in [1, 2] {
        let mut plugin = BilibiliPlugin {
            session: StoredSession::parse_cookie_input("SESSDATA=old-account", "device").unwrap(),
            ..Default::default()
        };
        let mut host = MockHost::default();
        let op = cookie_login(&mut plugin, &mut host, "SESSDATA=stale-login");
        logout(&mut plugin, &mut host, window);
        plugin
            .event(win(done(op, json!({"ok":{"stored":true}}))), &mut host, NOW)
            .unwrap();
        assert!(
            !plugin.session.is_authenticated(),
            "late write must not undo logout"
        );
        assert!(host.tasks.is_empty());
        assert!(host.closed.contains(&op));
        assert_eq!(plugin.session.buvid3, "device");

        let op = cookie_login(&mut plugin, &mut host, "SESSDATA=fresh-login");
        plugin
            .event(win(done(op, json!({"ok":{"stored":true}}))), &mut host, NOW)
            .unwrap();
        assert_eq!(plugin.session.sessdata, "fresh-login");
        assert!(plugin.session.is_authenticated());
    }
}

#[test]
fn logout_revokes_pending_qr_confirmation_in_every_window() {
    for window in [1, 2] {
        let mut plugin = BilibiliPlugin::default();
        plugin.session.sessdata = "old-account".into();
        let mut host = MockHost::default();
        plugin
            .event(win(action("start_qr")), &mut host, NOW)
            .unwrap();
        let (op, _) = host.pop();
        feed_http(&mut plugin, &mut host, op, 200, vec![],
            br#"{"code":0,"data":{"url":"https://passport.bilibili.com/fixture","qrcode_key":"key"}}"#);
        plugin
            .event(win(action("poll_qr")), &mut host, NOW)
            .unwrap();
        let (op, _) = host.pop();
        plugin
            .event(
                win(done(
                    op,
                    json!({"ok":{
                        "state":"response","stream_handle":"900","status":200,
                        "url":"https://passport.bilibili.com/fixture","headers":[],"written":null
                    }}),
                )),
                &mut host,
                NOW,
            )
            .unwrap();
        let (read, task) = host.pop();
        assert!(matches!(task, Task::HttpRead { .. }));
        logout(&mut plugin, &mut host, window);
        let blob = host.blob(br#"{"code":0,"data":{"code":0,"url":"https://passport.bilibili.com/fixture?SESSDATA=late-login"}}"#).unwrap();
        plugin
            .event(
                win(done(
                    read,
                    json!({"ok":{"eof":true,"blob":blob.0.to_string()}}),
                )),
                &mut host,
                NOW,
            )
            .unwrap();
        assert!(
            host.tasks.is_empty(),
            "late QR confirmation must not save credentials"
        );
        assert!(!plugin.session.is_authenticated());
        assert!(host.closed.contains(&read));
        assert!(host.closed.contains(&Id(900)));
        if window == 2 {
            assert!(
                host.closed.contains(&blob),
                "release the already delivered stale blob"
            );
        }
        assert!(plugin.windows.values().all(|state| state.qr.is_none()));
    }
}

#[test]
fn logout_revokes_the_initial_secret_read() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    plugin
        .event(
            HostEvent::Init {
                protocol: 2,
                abi: 2,
                host_version: "0.0.2".into(),
                role: Role::Control,
                generation: Generation::default(),
                capabilities: Default::default(),
                features: Default::default(),
                snapshot: None,
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (read, _) = host.pop();
    host.pop();
    logout(&mut plugin, &mut host, 1);
    let saved = StoredSession::parse_cookie_input("SESSDATA=old-account", "device").unwrap();
    plugin
        .event(
            done(
                read,
                json!({"ok":{"store_value":serde_json::to_string(&saved).unwrap()}}),
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(!plugin.session.is_authenticated());
    assert!(
        !host.closed.contains(&read),
        "a window cannot close a global operation"
    );
}

#[test]
fn logout_invalidates_private_catalog_continuations_in_every_window() {
    for window in [1, 2] {
        let mut plugin = BilibiliPlugin {
            session: StoredSession::parse_cookie_input(
                "SESSDATA=old-account; DedeUserID=1",
                "device",
            )
            .unwrap(),
            ..Default::default()
        };
        let mut host = MockHost::default();
        let browse = |page| {
            win(HostEvent::SourceBrowse {
                request_id: Id(500),
                source_id: "favorites".into(),
                folder_id: None,
                page,
                query: None,
                filters: BTreeMap::new(),
            })
        };
        plugin.event(browse(None), &mut host, NOW).unwrap();
        let (op, _) = host.pop();
        let folders: Vec<_> = (1..=61)
            .map(|id| json!({"id":id,"title":format!("private {id}")}))
            .collect();
        let commands = feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            vec![],
            &serde_json::to_vec(&json!({"code":0,"data":{"list":folders}})).unwrap(),
        );
        let cursor = commands
            .into_iter()
            .find_map(|cmd| match cmd {
                HostCommand::ProvideSourcePage { page, .. } => page.next_page,
                _ => None,
            })
            .expect("the remaining private folder needs a local cursor");
        logout(&mut plugin, &mut host, window);
        let commands = plugin.event(browse(Some(cursor)), &mut host, NOW).unwrap();
        assert!(
            commands.iter().any(|cmd| matches!(
                cmd,
                HostCommand::SourceError {
                    request_id: Id(500),
                    ..
                }
            )) && !commands
                .iter()
                .any(|cmd| matches!(cmd, HostCommand::ProvideSourcePage { .. })),
            "logout must invalidate the old account's private catalog tail"
        );
        assert!(host.tasks.is_empty());
    }
}

#[test]
fn logout_retires_browse_work_in_its_own_window_and_finishes_the_request() {
    for cancelled in [false, true] {
        let mut plugin = BilibiliPlugin {
            session: StoredSession::parse_cookie_input("SESSDATA=account; DedeUserID=1", "device")
                .unwrap(),
            ..Default::default()
        };
        let mut host = MockHost::default();
        plugin
            .event(
                win(HostEvent::SourceBrowse {
                    request_id: Id(500),
                    source_id: "favorites".into(),
                    folder_id: None,
                    page: None,
                    query: None,
                    filters: BTreeMap::new(),
                }),
                &mut host,
                NOW,
            )
            .unwrap();
        let (op, _) = host.pop();
        logout(&mut plugin, &mut host, 2);
        assert!(
            !host.closed.contains(&op),
            "window 2 cannot close window 1's operation"
        );
        let event = if cancelled {
            HostEvent::RequestCancelled {
                request_id: Id(500),
            }
        } else {
            done(
                op,
                json!({"ok":{"state":"response","stream_handle":"900","status":200,
                "url":"https://api.bilibili.com/fixture","headers":[],"written":null}}),
            )
        };
        let commands = plugin.event(win(event), &mut host, NOW).unwrap();
        assert_eq!(
            commands.iter().any(|cmd| matches!(
                cmd,
                HostCommand::SourceError {
                    request_id: Id(500),
                    ..
                }
            )),
            !cancelled
        );
        assert!(host.closed.contains(&op));
        if !cancelled {
            assert!(
                host.closed.contains(&Id(900)),
                "release the already delivered stale stream"
            );
        }
        assert!(
            host.tasks.is_empty(),
            "a stale response must not start reading its body"
        );
        assert!(plugin.windows[&Id(1)].http_jobs.is_empty());
    }
}

#[test]
fn logout_releases_late_subtitle_writers_and_artifacts_in_their_own_window() {
    for writing in [false, true] {
        let mut plugin = BilibiliPlugin::default();
        let mut host = MockHost::default();
        let mut state = WindowState {
            active_media: Some(Id(44)),
            ..Default::default()
        };
        plugin.stage_subtitle_bcc(
            &mut state,
            Id(44),
            SubtitleTrackMeta {
                id: "track".into(),
                lang: "zh".into(),
                label: "Chinese".into(),
                url: "https://i0.hdslb.com/fixture.json".into(),
            },
            br#"{"body":[{"from":0,"to":1,"content":"fixture"}]}"#,
            &mut host,
        );
        plugin.windows.insert(Id(1), state);
        let (mut op, task) = host.pop();
        assert!(matches!(task, Task::ArtifactBegin { .. }));
        if writing {
            plugin
                .event(
                    win(done(op, json!({"ok":{"artifact_writer":"901"}}))),
                    &mut host,
                    NOW,
                )
                .unwrap();
            let (write, task) = host.pop();
            assert!(matches!(task, Task::ArtifactWrite { .. }));
            op = write;
        }
        logout(&mut plugin, &mut host, 2);
        assert!(!host.closed.contains(&op));
        let result = if writing {
            json!({"ok":{"artifact":"902","bytes":"64","format":"web_vtt"}})
        } else {
            json!({"ok":{"artifact_writer":"901"}})
        };
        let commands = plugin.event(win(done(op, result)), &mut host, NOW).unwrap();
        assert!(host.closed.contains(&Id(901)));
        if writing {
            assert!(host.closed.contains(&Id(902)));
        }
        assert!(host.tasks.is_empty());
        assert!(
            !commands
                .iter()
                .any(|cmd| matches!(cmd, HostCommand::AttachSubtitle { .. }))
        );
        assert!(plugin.windows[&Id(1)].sub_jobs.is_empty());
    }
}

fn win_n(window: u64, event: HostEvent<Value>) -> HostEvent<Value> {
    HostEvent::Window {
        window: Id(window),
        generation: Generation::default(),
        body: Box::new(event),
    }
}

#[test]
fn account_switch_without_logout_purges_other_windows_and_replaces_pending_secret_write() {
    let mut plugin = BilibiliPlugin {
        session: StoredSession::parse_cookie_input("SESSDATA=acct-a; DedeUserID=101", "device")
            .unwrap(),
        ..Default::default()
    };
    let mut host = MockHost::default();

    // Window 1 caches an authenticated Bangumi follow page (> 60 items -> local cursor).
    let bangumi_filters = BTreeMap::from([("tab".into(), "follow_1".into())]);
    plugin
        .event(
            win_n(
                1,
                HostEvent::SourceBrowse {
                    request_id: Id(601),
                    source_id: "bangumi".into(),
                    folder_id: None,
                    page: None,
                    query: None,
                    filters: bangumi_filters.clone(),
                },
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    let (bg_op, _) = host.pop();
    let seasons: Vec<_> = (1..=61)
        .map(|id| json!({"season_id": id, "title": format!("follow {id}")}))
        .collect();
    let cmds = feed_http(
        &mut plugin,
        &mut host,
        bg_op,
        200,
        vec![],
        &serde_json::to_vec(&json!({"code": 0, "data": {"list": seasons}})).unwrap(),
    );
    let bg_cursor = cmds
        .into_iter()
        .find_map(|c| match c {
            HostCommand::ProvideSourcePage { page, .. } => page.next_page,
            _ => None,
        })
        .expect("expected local cursor for bangumi follow");

    // Window 2 caches an authenticated Live following page AND starts an in-flight favorites browse.
    let live_filters = BTreeMap::from([("tab".into(), "following".into())]);
    plugin
        .event(
            win_n(
                2,
                HostEvent::SourceBrowse {
                    request_id: Id(602),
                    source_id: "live".into(),
                    folder_id: None,
                    page: None,
                    query: None,
                    filters: live_filters.clone(),
                },
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    let (live_op, _) = host.pop();
    plugin
        .event(
            win_n(
                2,
                done(
                    live_op,
                    json!({
                        "ok": {
                            "state": "response",
                            "stream_handle": "950",
                            "status": 200,
                            "url": "https://api.live.bilibili.com/ok",
                            "headers": [],
                            "written": null
                        }
                    }),
                ),
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    let (live_read, _) = host.pop();
    let rooms: Vec<_> = (1..=61)
        .map(|id| json!({"roomid": id, "title": format!("room {id}"), "live_status": 1}))
        .collect();
    let live_blob = host
        .blob(&serde_json::to_vec(&json!({"code": 0, "data": {"list": rooms}})).unwrap())
        .unwrap();
    let live_cmds = plugin
        .event(
            win_n(
                2,
                done(
                    live_read,
                    json!({"ok": {"eof": true, "text": null, "blob": live_blob.0.to_string()}}),
                ),
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    let live_cursor = live_cmds
        .into_iter()
        .find_map(|c| match c {
            HostCommand::ProvideSourcePage { page, .. } => page.next_page,
            _ => None,
        })
        .expect("expected local cursor for live following");

    plugin
        .event(
            win_n(
                2,
                HostEvent::SourceBrowse {
                    request_id: Id(603),
                    source_id: "favorites".into(),
                    folder_id: None,
                    page: None,
                    query: None,
                    filters: BTreeMap::new(),
                },
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    let (w2_inflight_op, _) = host.pop();

    // Window 1 starts cookie login #1, then immediately overwrites it with cookie login #2.
    let write_op1 = cookie_login(&mut plugin, &mut host, "SESSDATA=acct-b1; DedeUserID=201");
    let write_op2 = cookie_login(&mut plugin, &mut host, "SESSDATA=acct-b2; DedeUserID=202");
    assert!(
        host.closed.contains(&write_op1),
        "overwriting secret_write_op must close the superseded handle"
    );

    // Commit Account B2 directly without prior logout.
    plugin
        .event(
            win_n(1, done(write_op2, json!({"ok": {"stored": true}}))),
            &mut host,
            NOW,
        )
        .unwrap();
    let (_nav_op, _) = host.pop();
    assert_eq!(plugin.session.sessdata, "acct-b2");

    // Window 1's cached Bangumi follow cursor from Account A must be rejected.
    let w1_stale = plugin
        .event(
            win_n(
                1,
                HostEvent::SourceBrowse {
                    request_id: Id(604),
                    source_id: "bangumi".into(),
                    folder_id: None,
                    page: Some(bg_cursor),
                    query: None,
                    filters: bangumi_filters,
                },
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(matches!(
        w1_stale.as_slice(),
        [HostCommand::SourceError {
            request_id: Id(604),
            ..
        }]
    ));

    // Window 2 flushes its account reset on next event: cancels in-flight browse, emits SourceError(603),
    // and rejects Account A's cached Live following cursor.
    let w2_cmds = plugin
        .event(
            win_n(
                2,
                HostEvent::SourceBrowse {
                    request_id: Id(605),
                    source_id: "live".into(),
                    folder_id: None,
                    page: Some(live_cursor),
                    query: None,
                    filters: live_filters,
                },
            ),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(host.closed.contains(&w2_inflight_op));
    assert!(w2_cmds.iter().any(|c| matches!(
        c,
        HostCommand::SourceError {
            request_id: Id(603),
            ..
        }
    )));
    assert!(w2_cmds.iter().any(|c| matches!(
        c,
        HostCommand::SourceError {
            request_id: Id(605),
            ..
        }
    )));
    assert!(
        !w2_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::ProvideSourcePage { .. }))
    );
}

#[test]
fn percent_encoded_plugin_uris_and_fifo_cid_cache_preserve_enhancements_and_clear_on_logout() {
    let mut plugin = BilibiliPlugin {
        wbi: auth::WbiKeys {
            img_key: "7cd084941338484aae1ad9425b84077c".into(),
            sub_key: "4932caff0ff746eab6f01bf08b70ac45".into(),
            updated_secs: 1_700_000,
        },
        ..Default::default()
    };
    let mut host = MockHost::default();

    // Fill CidCache past its 256-entry cap with high-ASCII `ugc:` keys.
    let mut dummy_state = WindowState::default();
    for i in 0..150u64 {
        let bvid = format!("BV1zz411c{:03}", i);
        let meta = playback::VideoMeta {
            bvid: bvid.clone(),
            aid: 10_000 + i,
            cid: 20_000 + i,
            ep_id: None,
            title: format!("Multi {i}"),
            source_id: "recommend".into(),
            entry_id: format!("ugc:{bvid}:{}", 20_000 + i),
            playlist: Vec::new(),
        };
        plugin.remember_cid(&mut dummy_state, &meta, &meta.entry_id);
    }
    assert!(plugin.cid_lookup.len() <= 256);

    // Resolve a plain `BV...` catalog item and a Bangumi `ep:...` item after `ugc:` saturation.
    let bv_meta = playback::VideoMeta {
        bvid: "BV1AA411c7mD".into(),
        aid: 501,
        cid: 601,
        ep_id: None,
        title: "Single BV".into(),
        source_id: "popular".into(),
        entry_id: "BV1AA411c7mD".into(),
        playlist: Vec::new(),
    };
    plugin.remember_cid(&mut dummy_state, &bv_meta, "BV1AA411c7mD");

    let ep_meta = playback::VideoMeta {
        bvid: "BV1BB411c7mD".into(),
        aid: 502,
        cid: 602,
        ep_id: Some(754_321),
        title: "Bangumi EP".into(),
        source_id: "bangumi".into(),
        entry_id: "ep:754321".into(),
        playlist: Vec::new(),
    };
    plugin.remember_cid(&mut dummy_state, &ep_meta, "ss:12345");

    // Newly inserted BV and percent-encoded ep/ss/ugc MediaUri::Plugin strings must all resolve.
    assert_eq!(
        plugin.lookup_playing_cid("iris-plugin://community.bilibili/popular/BV1AA411c7mD"),
        Some(("BV1AA411c7mD".into(), 501, 601))
    );
    assert_eq!(
        plugin.lookup_playing_cid("iris-plugin://community.bilibili/bangumi/ep%3A754321"),
        Some(("BV1BB411c7mD".into(), 502, 602))
    );
    assert_eq!(
        plugin.lookup_playing_cid("iris-plugin://community.bilibili/bangumi/ss%3A12345"),
        Some(("BV1BB411c7mD".into(), 502, 602))
    );
    assert_eq!(
        plugin.lookup_playing_cid(
            "iris-plugin://community.bilibili/recommend/ugc%3ABV1uv411q7Mv%3A777"
        ),
        Some(("BV1uv411q7Mv".into(), 0, 777))
    );
    assert_eq!(
        plugin.lookup_playing_cid("plugin://community.bilibili/recommend/ugc%3Aav9988%3A666"),
        Some((String::new(), 9988, 666))
    );
    assert_eq!(
        plugin.lookup_playing_cid("iris-plugin://iris.webdav/nas/ep%3A754321"),
        None
    );

    // MediaOpened with percent-encoded Bangumi URI starts Danmaku + PlayerV2.
    plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(70)),
                    uri: Some("iris-plugin://community.bilibili/bangumi/ep%3A754321".into()),
                    ..Default::default()
                },
            }),
            &mut host,
            NOW,
        )
        .unwrap();
    let (_, dm_task) = host.pop();
    let (_, pv2_task) = host.pop();
    assert!(matches!(dm_task, Task::HttpStart { url, .. } if url.contains("oid=602")));
    assert!(matches!(pv2_task, Task::HttpStart { url, .. } if url.contains("cid=602")));

    // Logout clears CidCache and per-window last_resolved_cid.
    plugin.windows.get_mut(&Id(1)).unwrap().last_resolved_cid =
        Some(("BV1AA411c7mD".into(), 501, 601));
    logout(&mut plugin, &mut host, 1);
    assert!(plugin.cid_lookup.is_empty());
    assert!(plugin.windows[&Id(1)].last_resolved_cid.is_none());
}

#[test]
fn bare_sessdata_login_waits_for_nav_persists_mid_and_filters_root_favorites() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();

    let sec_op = cookie_login(&mut plugin, &mut host, "SESSDATA=bare_only_token");
    let login_cmds = plugin
        .event(
            win(done(sec_op, json!({"ok":{"stored":true}}))),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(
        !login_cmds.iter().any(
            |c| matches!(c, HostCommand::RefreshSource { source_id } if source_id == "favorites")
        ),
        "bare-SESSDATA login must wait for Nav mid before refreshing favorites"
    );
    let (nav_op, nav_task) = host.pop();
    assert!(
        matches!(nav_task, Task::HttpStart { url, .. } if url.contains("/x/web-interface/nav"))
    );

    // Browsing root favorites with a search query before Nav finishes queues cleanly behind Nav.
    let queued = plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(801),
                source_id: "favorites".into(),
                folder_id: None,
                page: None,
                query: Some("ost".into()),
                filters: BTreeMap::new(),
            }),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(
        queued.is_empty(),
        "favorites browse during in-flight Nav must wait instead of failing: {queued:?}"
    );
    assert!(host.tasks.is_empty());

    // Nav resolves `mid` -> persists updated session to vault, replays queued favorites, and refreshes bangumi.
    let nav_cmds = feed_http(
        &mut plugin,
        &mut host,
        nav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":true,"mid":334455,"uname":"BareUser","wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );
    assert_eq!(plugin.session.dede_user_id, "334455");
    assert!(
        nav_cmds.iter().any(
            |c| matches!(c, HostCommand::RefreshSource { source_id } if source_id == "bangumi")
        )
    );
    let (bg_sec_op, bg_sec_task) = host.pop();
    let Task::SecretWrite {
        value: Some(saved_json),
        ..
    } = bg_sec_task
    else {
        panic!("expected background SecretWrite persisting discovered mid");
    };
    assert!(saved_json.contains("334455"));
    plugin
        .event(
            win(done(bg_sec_op, json!({"ok":{"stored":true}}))),
            &mut host,
            NOW,
        )
        .unwrap();

    let (fav_op, fav_task) = host.pop();
    assert!(
        matches!(fav_task, Task::HttpStart { url, .. } if url.contains("up_mid=334455")),
        "queued favorites must dispatch with Nav-discovered mid"
    );
    let fav_cmds = feed_http(
        &mut plugin,
        &mut host,
        fav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"list":[{"id":1,"title":"Anime OST Collection","media_count":12},{"id":2,"title":"Tech Talks","media_count":5}]}}"#,
    );
    let [
        HostCommand::ProvideSourcePage {
            request_id: Id(801),
            page,
        },
    ] = fav_cmds.as_slice()
    else {
        panic!("expected filtered favorites page, got {fav_cmds:?}");
    };
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].id, "fav:1");
    assert_eq!(page.entries[0].name, "Anime OST Collection");
}

#[test]
fn media_generation_advance_resubmits_interrupted_login_and_logout_secret_writes() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();

    // 1. Login SecretWrite interrupted by media generation advance is re-submitted without closing stale op.
    let gen0_login_op = cookie_login(
        &mut plugin,
        &mut host,
        "SESSDATA=gen-login; DedeUserID=7788",
    );
    let gen1 = Generation {
        media: Id(1),
        ..Generation::default()
    };
    plugin
        .event(
            HostEvent::Window {
                window: Id(1),
                generation: gen1,
                body: Box::new(HostEvent::MediaOpened {
                    generation: gen1,
                    snapshot: Default::default(),
                }),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(
        !host.closed.contains(&gen0_login_op),
        "must not close host-revoked old-generation handle"
    );
    let (gen1_login_op, retry_task) = host.pop();
    assert!(matches!(
        retry_task,
        Task::SecretWrite { value: Some(_), .. }
    ));

    let commit_cmds = plugin
        .event(
            HostEvent::Window {
                window: Id(1),
                generation: gen1,
                body: Box::new(done(gen1_login_op, json!({"ok":{"stored":true}}))),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (_nav_op, _) = host.pop();
    assert_eq!(plugin.session.sessdata, "gen-login");
    assert!(commit_cmds.iter().any(|c| matches!(
        c,
        HostCommand::CloseModal { id: Some(id) } if id == "bili.login"
    )));

    // 2. Logout SecretWrite(None) interrupted by media generation advance is also re-submitted.
    plugin
        .event(
            HostEvent::Window {
                window: Id(1),
                generation: gen1,
                body: Box::new(action("logout")),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (gen1_logout_op, logout_task) = host.pop();
    assert!(matches!(logout_task, Task::SecretWrite { value: None, .. }));

    let gen2 = Generation {
        media: Id(2),
        ..Generation::default()
    };
    plugin
        .event(
            HostEvent::Window {
                window: Id(1),
                generation: gen2,
                body: Box::new(HostEvent::MediaOpened {
                    generation: gen2,
                    snapshot: Default::default(),
                }),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(!host.closed.contains(&gen1_logout_op));
    let (_gen2_logout_op, retry_logout_task) = host.pop();
    assert!(matches!(
        retry_logout_task,
        Task::SecretWrite { value: None, .. }
    ));
}
