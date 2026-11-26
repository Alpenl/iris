use super::*;

const NOW: u64 = 1_700_000_000_000;

fn action(id: &str) -> HostEvent<Value> {
    HostEvent::Action {
        id: id.into(),
        value: None,
        snapshot: None,
    }
}

fn logout(plugin: &mut BaiduPanPlugin, host: &mut MockHost, window: u64) {
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

fn cookie_login(plugin: &mut BaiduPanPlugin, host: &mut MockHost) -> Id {
    plugin
        .event(
            win(HostEvent::ModalSubmit {
                id: "baidupan.login".into(),
                values: BTreeMap::from([("cookie".into(), "BDUSS=new-account".into())]),
            }),
            host,
            NOW,
        )
        .unwrap();
    let (op, task) = host.pop();
    assert!(matches!(task, Task::HttpStart { .. }));
    op
}

fn pending_read(plugin: &mut BaiduPanPlugin, host: &mut MockHost, op: Id) -> Id {
    plugin
        .event(
            win(done(
                op,
                json!({"ok":{"state":"response","stream_handle":"900","status":200,
                    "url":"https://pan.baidu.com/fixture","headers":[],"written":null}}),
            )),
            host,
            NOW,
        )
        .unwrap();
    let (read, task) = host.pop();
    assert!(matches!(task, Task::HttpRead { .. }));
    read
}

fn finish_read(plugin: &mut BaiduPanPlugin, host: &mut MockHost, op: Id, body: &[u8]) -> Id {
    let blob = host.blob(body).unwrap();
    plugin
        .event(
            win(done(
                op,
                json!({"ok":{"eof":true,"blob":blob.0.to_string()}}),
            )),
            host,
            NOW,
        )
        .unwrap();
    blob
}

#[test]
fn logout_revokes_pending_login_verification_in_every_window() {
    for window in [1, 2] {
        let mut plugin = BaiduPanPlugin::default();
        plugin.session.bduss = "old-account".into();
        let mut host = MockHost::default();
        let op = cookie_login(&mut plugin, &mut host);
        let read = pending_read(&mut plugin, &mut host, op);
        logout(&mut plugin, &mut host, window);
        let blob = finish_read(
            &mut plugin,
            &mut host,
            read,
            br#"{"errno":0,"user_info":{"uk":123,"username":"new-account"}}"#,
        );
        assert!(
            host.tasks.is_empty(),
            "late verification must not save credentials"
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
    }
}

#[test]
fn logout_revokes_pending_secret_write_and_allows_a_fresh_login() {
    for window in [1, 2] {
        let mut plugin = BaiduPanPlugin::default();
        plugin.session.bduss = "old-account".into();
        let mut host = MockHost::default();
        let op = cookie_login(&mut plugin, &mut host);
        feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            "https://pan.baidu.com/fixture",
            vec![],
            br#"{"errno":0,"user_info":{"uk":123,"username":"new-account"}}"#,
        );
        let (write, task) = host.pop();
        assert!(matches!(task, Task::SecretWrite { value: Some(_), .. }));
        logout(&mut plugin, &mut host, window);
        plugin
            .event(
                win(done(write, json!({"ok":{"stored":true}}))),
                &mut host,
                NOW,
            )
            .unwrap();
        assert!(
            !plugin.session.is_authenticated(),
            "late write must not undo logout"
        );
        assert!(host.tasks.is_empty());
        assert!(host.closed.contains(&write));

        let op = cookie_login(&mut plugin, &mut host);
        feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            "https://pan.baidu.com/fixture",
            vec![],
            br#"{"errno":0,"user_info":{"uk":456,"username":"fresh-login"}}"#,
        );
        let (write, _) = host.pop();
        plugin
            .event(
                win(done(write, json!({"ok":{"stored":true}}))),
                &mut host,
                NOW,
            )
            .unwrap();
        assert!(plugin.session.is_authenticated());
        assert_eq!(plugin.session.uid, 456);
    }
}

#[test]
fn logout_revokes_pending_oauth_refresh_before_it_can_rewrite_the_vault() {
    for window in [1, 2] {
        let mut plugin = BaiduPanPlugin {
            session: StoredSession::parse_openapi_input("expired", "refresh", "client", "secret")
                .unwrap(),
            ..Default::default()
        };
        let mut host = MockHost::default();
        plugin
            .event(win(action("refresh_account")), &mut host, NOW)
            .unwrap();
        let (op, _) = host.pop();
        feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            "https://pan.baidu.com/fixture",
            vec![],
            br#"{"errno":111}"#,
        );
        let (refresh, task) = host.pop();
        assert!(matches!(task, Task::HttpStart { url, .. } if url.contains("oauth/2.0/token")));
        let read = pending_read(&mut plugin, &mut host, refresh);
        logout(&mut plugin, &mut host, window);
        finish_read(
            &mut plugin,
            &mut host,
            read,
            br#"{"access_token":"late-token","refresh_token":"late-refresh"}"#,
        );
        assert!(
            !plugin.session.is_authenticated(),
            "late refresh must not undo logout"
        );
        assert!(
            host.tasks.is_empty(),
            "late refresh must not rewrite deleted credentials"
        );
        assert!(host.closed.contains(&read));
        assert!(host.closed.contains(&Id(900)));
    }
}

#[test]
fn logout_revokes_the_initial_secret_read() {
    let mut plugin = BaiduPanPlugin::default();
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
    let saved = StoredSession::parse_cookie_input("BDUSS=old-account").unwrap();
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
fn logout_retires_browse_work_in_its_own_window_and_finishes_the_request() {
    for cancelled in [false, true] {
        let mut plugin = BaiduPanPlugin::default();
        plugin.session.bduss = "account".into();
        let mut host = MockHost::default();
        plugin
            .event(
                win(HostEvent::SourceBrowse {
                    request_id: Id(500),
                    source_id: "files".into(),
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
                "url":"https://pan.baidu.com/fixture","headers":[],"written":null}}),
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
fn direct_account_switch_purges_caches_and_discards_stale_account_refresh() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession::parse_openapi_input("old_at", "old_rt", "cid", "csec").unwrap(),
        quota: Some(auth::QuotaInfo {
            total: 100,
            used: 50,
        }),
        ..Default::default()
    };
    plugin.video_lookup.insert(
        "bd:1:/PrivateA.mp4".into(),
        ("/PrivateA.mp4".into(), "PrivateA.mp4".into()),
    );
    let mut host = MockHost::default();

    // Window 1 populates a local pagination cache and folder cache under Account A.
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(1),
                source_id: "files".into(),
                folder_id: Some("/Secret".into()),
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            NOW,
        )
        .unwrap();
    let (b_op, _) = host.pop();
    let items: Vec<_> = (1..=65)
        .map(|n| {
            json!({
                "path":format!("/Secret/vid{n}.mp4"),
                "fs_id":n,
                "category":1,
                "isdir":0,
                "server_filename":format!("vid{n}.mp4")
            })
        })
        .collect();
    let first_cmds = feed_http(
        &mut plugin,
        &mut host,
        b_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        &serde_json::to_vec(&json!({"errno":0,"list":items})).unwrap(),
    );
    let old_cursor = match &first_cmds[0] {
        HostCommand::ProvideSourcePage { page, .. } => page.next_page.clone().unwrap(),
        other => panic!("expected page, got {other:?}"),
    };
    plugin.windows.get_mut(&Id(1)).unwrap().last_resolved_video =
        Some(("/Secret/vid1.mp4".into(), "vid1.mp4".into()));

    // Window 2 starts an OAuth refresh for Account A before Account B logs in on Window 1.
    plugin
        .event(
            HostEvent::Window {
                window: Id(2),
                generation: Generation::default(),
                body: Box::new(action("refresh_account")),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (uinfo_w2, _) = host.pop();
    plugin
        .event(
            HostEvent::Window {
                window: Id(2),
                generation: Generation::default(),
                body: Box::new(done(
                    uinfo_w2,
                    json!({"ok":{"state":"response","stream_handle":"920","status":200,
                        "url":"https://pan.baidu.com/rest/2.0/xpan/nas","headers":[],"written":null}}),
                )),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (read_w2, _) = host.pop();
    let exp_blob = host.blob(br#"{"errno":111}"#).unwrap();
    plugin
        .event(
            HostEvent::Window {
                window: Id(2),
                generation: Generation::default(),
                body: Box::new(done(
                    read_w2,
                    json!({"ok":{"eof":true,"blob":exp_blob.0.to_string()}}),
                )),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    let (refresh_w2, _) = host.pop();

    // Window 1 directly logs into Account B (without clicking logout first), and double-submits to test SecretWrite replacement.
    let op_b1 = cookie_login(&mut plugin, &mut host);
    feed_http(
        &mut plugin,
        &mut host,
        op_b1,
        200,
        "https://pan.baidu.com/fixture",
        vec![],
        br#"{"errno":0,"user_info":{"uk":701,"username":"AccountB1"}}"#,
    );
    let (write_b1, _) = host.pop();

    let op_b2 = cookie_login(&mut plugin, &mut host);
    feed_http(
        &mut plugin,
        &mut host,
        op_b2,
        200,
        "https://pan.baidu.com/fixture",
        vec![],
        br#"{"errno":0,"user_info":{"uk":702,"username":"AccountB2"}}"#,
    );
    let (write_b2, _) = host.pop();
    assert!(
        host.closed.contains(&write_b1),
        "overwriting secret_write_op must close the superseded SecretWrite handle"
    );

    plugin
        .event(
            win(done(write_b2, json!({"ok":{"stored":true}}))),
            &mut host,
            NOW,
        )
        .unwrap();
    let (_quota_op, _) = host.pop();
    assert_eq!(plugin.session.uid, 702);
    assert!(plugin.quota.is_none(), "Account A quota must be cleared");
    assert!(
        plugin.video_lookup.is_empty(),
        "Account A video_lookup must be cleared"
    );
    assert!(plugin.windows[&Id(1)].folder_caches.is_empty());
    assert!(plugin.windows[&Id(1)].browse_page.is_none());
    assert!(plugin.windows[&Id(1)].last_resolved_video.is_none());
    assert!(plugin.windows[&Id(2)].account_reset);

    // Account A's local cursor in Window 1 must be rejected even though Account B is authenticated.
    let cont = plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(99),
                source_id: "files".into(),
                folder_id: Some("/Secret".into()),
                page: Some(old_cursor),
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            NOW,
        )
        .unwrap();
    assert!(matches!(cont.as_slice(), [HostCommand::SourceError { .. }]));

    // Late completion of Window 2's Account A OAuth refresh is retired and cannot overwrite Account B.
    plugin
        .event(
            HostEvent::Window {
                window: Id(2),
                generation: Generation::default(),
                body: Box::new(done(
                    refresh_w2,
                    json!({"ok":{"state":"response","stream_handle":"921","status":200,
                        "url":"https://openapi.baidu.com/oauth/2.0/token","headers":[],"written":null}}),
                )),
            },
            &mut host,
            NOW,
        )
        .unwrap();
    assert_eq!(plugin.session.uid, 702);
    assert!(host.closed.contains(&Id(921)));
    assert!(host.tasks.is_empty());

    // Explicit logout also clears video_lookup and last_resolved_video.
    plugin.video_lookup.insert(
        "bd:2:/PrivateB.mp4".into(),
        ("/PrivateB.mp4".into(), "PrivateB.mp4".into()),
    );
    plugin.windows.get_mut(&Id(1)).unwrap().last_resolved_video =
        Some(("/PrivateB.mp4".into(), "PrivateB.mp4".into()));
    logout(&mut plugin, &mut host, 1);
    assert!(plugin.video_lookup.is_empty());
    assert!(plugin.windows[&Id(1)].last_resolved_video.is_none());
}
