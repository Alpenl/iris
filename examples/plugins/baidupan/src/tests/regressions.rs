use super::*;

#[test]
fn browse_responses_echo_root_folder_and_search_request_identities() {
    for (folder, query, body) in [
        (None, None, r#"{"errno":0,"list":[]}"#),
        (Some("/"), None, r#"{"errno":0,"list":[]}"#),
        (Some("/Movies/"), None, r#"{"errno":0,"list":[]}"#),
        (Some("/Movies"), Some("movie"), r#"{"errno":0,"list":[]}"#),
        (Some("/Movies"), None, r#"{"errno":-6}"#),
    ] {
        let mut plugin = BaiduPanPlugin::default();
        plugin.session.bduss = "fixture-session".into();
        let mut host = MockHost::default();
        let request_id = Id(20);
        plugin
            .event(
                win(HostEvent::SourceBrowse {
                    request_id,
                    source_id: "files".into(),
                    folder_id: folder.map(str::to_owned),
                    page: None,
                    query: query.map(str::to_owned),
                    filters: BTreeMap::new(),
                }),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap();
        let (op, _) = host.pop();
        let commands = feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            "https://pan.baidu.com/api/list",
            vec![],
            body.as_bytes(),
        );
        let [
            HostCommand::ProvideSourcePage {
                request_id: actual,
                page,
            },
        ] = commands.as_slice()
        else {
            panic!("expected a source page, got {commands:?}");
        };
        assert_eq!(*actual, request_id);
        assert_eq!(page.folder_id.as_deref(), folder);
    }
}

#[test]
fn generation_change_discards_scoped_state_without_closing_revoked_handles() {
    let mut plugin = BaiduPanPlugin::default();
    plugin.session.bduss = "fixture-session".into();
    let mut host = MockHost::default();
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(10),
                source_id: "files".into(),
                folder_id: None,
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (op, _) = host.pop();
    assert!(plugin.windows[&Id(1)].http_jobs.contains_key(&op));

    let next_gen = Generation {
        media: Id(1),
        ..Generation::default()
    };
    plugin
        .event(
            HostEvent::Window {
                window: Id(1),
                generation: next_gen,
                body: Box::new(HostEvent::WindowOpened { snapshot: None }),
            },
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(
        host.closed.is_empty(),
        "must not call host.close on host-revoked handles from previous generation"
    );
    assert!(!plugin.windows[&Id(1)].http_jobs.contains_key(&op));
    assert_eq!(plugin.windows[&Id(1)].generation, next_gen);
}

#[test]
fn openapi_head_backpressure_falls_back_to_authenticated_dlink_for_media_and_subtitles() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession {
            mode: auth::AuthMode::OpenApi,
            access_token: "TOKEN_123".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = MockHost::default();

    // 1. Media resolve: fill all 6 HTTP slots before HEAD submission so HEAD fails with Backpressure.
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(71),
                source_id: "files".into(),
                folder_id: Some("/".into()),
                entry_id: "bd:777:/Movie.mp4".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (meta_op, _) = host.pop();
    plugin
        .event(
            win(done(
                meta_op,
                json!({"ok":{"state":"response","stream_handle":"900","status":200,
                    "url":"https://pan.baidu.com/rest/2.0/xpan/multimedia","headers":[],"written":null}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (read_op, _) = host.pop();
    // Fill 6 extra slots so `start_http("HEAD")` hits `http_jobs.len() >= 6` Backpressure
    // even after `read_op` is removed by `handle_completed`.
    {
        let state = plugin.windows.get_mut(&Id(1)).unwrap();
        for idx in 1..=6u64 {
            state.http_jobs.insert(
                Id(10_000 + idx),
                ActiveHttp {
                    op: Id(10_000 + idx),
                    stream: None,
                    status: 0,
                    final_url: String::new(),
                    headers: Vec::new(),
                    body: Vec::new(),
                    request_id: None,
                    flow: HttpFlow::Quota { account_epoch: 0 },
                },
            );
        }
    }
    let blob = host
        .blob(br#"{"errno":0,"list":[{"dlink":"https://d.pcs.baidu.com/file/vid?fid=777"}]}"#)
        .unwrap();
    let cmds = plugin
        .event(
            win(done(
                read_op,
                json!({"ok":{"eof":true,"text":null,"blob":blob.0.to_string()}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let [HostCommand::ResolveStream { media, .. }] = cmds.as_slice() else {
        panic!("expected ResolveStream fallback, got {cmds:?}");
    };
    let StreamSource::DirectHttp { primary, .. } = &media.stream.source else {
        panic!("expected DirectHttp");
    };
    assert!(
        primary.url.contains("access_token=TOKEN_123"),
        "fallback dlink must keep access_token: {}",
        primary.url
    );

    // 2. Subtitle resolve: HEAD backpressure attaches subtitle with authenticated `with_token`.
    plugin.windows.get_mut(&Id(1)).unwrap().http_jobs.clear();
    plugin
        .windows
        .get_mut(&Id(1))
        .unwrap()
        .folder_caches
        .insert(
            "/".into(),
            catalog::FolderCache {
                media: Vec::new(),
                subtitles: vec![catalog::CachedSubtitle {
                    fs_id: 888,
                    path: "/Movie.srt".into(),
                    name: "Movie.srt".into(),
                    format: SubtitleFormat::SubRip,
                    lang: Some("zh".into()),
                }],
            },
        );
    plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(99)),
                    uri: Some("iris-plugin://community.baidupan/files/bd:777:/Movie.mp4".into()),
                    ..Default::default()
                },
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (sub_meta_op, _) = host.pop();
    plugin
        .event(
            win(done(
                sub_meta_op,
                json!({"ok":{"state":"response","stream_handle":"901","status":200,
                    "url":"https://pan.baidu.com/rest/2.0/xpan/multimedia","headers":[],"written":null}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (sub_read_op, _) = host.pop();
    {
        let state = plugin.windows.get_mut(&Id(1)).unwrap();
        for idx in 1..=6u64 {
            state.http_jobs.insert(
                Id(20_000 + idx),
                ActiveHttp {
                    op: Id(20_000 + idx),
                    stream: None,
                    status: 0,
                    final_url: String::new(),
                    headers: Vec::new(),
                    body: Vec::new(),
                    request_id: None,
                    flow: HttpFlow::Quota { account_epoch: 0 },
                },
            );
        }
    }
    let sub_blob = host
        .blob(br#"{"errno":0,"list":[{"dlink":"https://d.pcs.baidu.com/file/sub?fid=888"}]}"#)
        .unwrap();
    let sub_cmds = plugin
        .event(
            win(done(
                sub_read_op,
                json!({"ok":{"eof":true,"text":null,"blob":sub_blob.0.to_string()}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        sub_cmds.as_slice(),
        [HostCommand::AttachSubtitle {
            resource: iris_plugin_sdk::api::v2::enhance::EnhancementResource::Remote { request },
            ..
        }] if request.url.contains("access_token=TOKEN_123")
    ));
}

#[test]
fn oauth_retry_preserves_original_none_variant_for_openapi_and_streaming() {
    for playback_mode in ["direct", "transcode_1080"] {
        let mut plugin = BaiduPanPlugin {
            session: StoredSession {
                mode: auth::AuthMode::OpenApi,
                access_token: "EXPIRED_AT".into(),
                refresh_token: "RT_OLD".into(),
                client_id: "APP_KEY".into(),
                client_secret: "APP_SEC".into(),
                ..Default::default()
            },
            prefs: auth::Preferences {
                playback_mode: playback_mode.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut host = MockHost::default();
        plugin
            .event(
                win(HostEvent::SourceResolve {
                    request_id: Id(81),
                    source_id: "files".into(),
                    folder_id: Some("/".into()),
                    entry_id: "bd:777:/Movie.mp4".into(),
                    variant: None,
                }),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap();
        let (first_op, _) = host.pop();
        let cmds = feed_http(
            &mut plugin,
            &mut host,
            first_op,
            200,
            "https://pan.baidu.com/rest/2.0/xpan/file",
            vec![],
            br#"{"errno":111,"errmsg":"Access token expired"}"#,
        );
        assert!(cmds.is_empty());
        let (refresh_op, _) = host.pop();
        let retry_variant = match &plugin.windows[&Id(1)].http_jobs[&refresh_op].flow {
            HttpFlow::OAuthRefresh { retry, .. } => match retry.as_ref() {
                OAuthRetry::Resolve { variant, .. } => variant.clone(),
                _ => panic!("expected OAuthRetry::Resolve"),
            },
            _ => panic!("expected OAuthRefresh"),
        };
        assert_eq!(
            retry_variant, None,
            "OAuthRetry::Resolve must preserve original variant: None for {playback_mode}"
        );
    }
}

#[test]
fn zero_clock_reports_error_and_logs_instead_of_clamping_to_fake_timestamp() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession {
            mode: auth::AuthMode::Cookie,
            bduss: "valid-bduss".into(),
            uid: 12345,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = MockHost::default();
    let cmds = plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(91),
                source_id: "files".into(),
                folder_id: Some("/".into()),
                entry_id: "bd:1:/Movie.mp4".into(),
                variant: None,
            }),
            &mut host,
            0,
        )
        .unwrap();
    assert!(
        host.tasks.is_empty(),
        "must not issue signed Locate request with fake clock"
    );
    assert!(cmds.iter().any(|c| matches!(c, HostCommand::Log { .. })));
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::SourceError {
            request_id: Id(91),
            ..
        }
    )));
}

#[test]
fn qr_png_rejects_corrupted_chunk_crc() {
    let (mut png, _) = build_synthetic_qr_png(21, 4);
    assert!(qr_png::decode_qr_png_to_matrix(&png).is_ok());
    // Flip a bit in the IHDR CRC field (signature 8B + len 4B + type 4B + IHDR data 13B = offset 29).
    png[29] ^= 0x01;
    let err = qr_png::decode_qr_png_to_matrix(&png).unwrap_err();
    assert!(
        err.contains("CRC"),
        "expected CRC validation error, got: {err}"
    );
}

#[test]
fn percent_encoded_uri_attaches_subtitles_and_unrelated_uri_never_consumes_stale_fallback() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession {
            mode: auth::AuthMode::Cookie,
            bduss: "BDUSS_OK".into(),
            uid: 99,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = MockHost::default();

    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(1),
                source_id: "files".into(),
                folder_id: Some("/Movies/Sci Fi".into()),
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (list_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        list_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        r#"{"errno":0,"list":[
            {"fs_id":11,"path":"/Movies/Sci Fi/第1话: 启程.mkv","server_filename":"第1话: 启程.mkv","isdir":0,"size":1000,"category":1,"server_mtime":1700000000},
            {"fs_id":12,"path":"/Movies/Sci Fi/第2话: 远航.mkv","server_filename":"第2话: 远航.mkv","isdir":0,"size":1000,"category":1,"server_mtime":1700000001},
            {"fs_id":13,"path":"/Movies/Sci Fi/第1话: 启程.chs.srt","server_filename":"第1话: 启程.chs.srt","isdir":0,"size":128,"category":6,"server_mtime":1700000000}
        ]}"#
        .as_bytes(),
    );

    // Resolve via raw path: StreamSpec.entry_id must be canonicalized to `bd:11:...` matching playlist.
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(2),
                source_id: "files".into(),
                folder_id: Some("/Movies/Sci Fi".into()),
                entry_id: "/Movies/Sci Fi/第1话: 启程.mkv".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (loc_op, _) = host.pop();
    let res_cmds = feed_http(
        &mut plugin,
        &mut host,
        loc_op,
        200,
        "https://pcs.baidu.com/rest/2.0/pcs/file",
        vec![],
        br#"{"errno":0,"urls":[{"url":"https://qdall01.baidupcs.com/file/ep1","encrypt":0}]}"#,
    );
    let [HostCommand::ResolveStream { media, .. }] = res_cmds.as_slice() else {
        panic!("expected ResolveStream");
    };
    assert_eq!(
        media.stream.entry_id.as_deref(),
        Some("bd:11:/Movies/Sci Fi/第1话: 启程.mkv")
    );
    assert_eq!(
        media.playlist[0].entry_id,
        "bd:11:/Movies/Sci Fi/第1话: 启程.mkv"
    );

    // Suppose that playback was superseded by a local file: must NOT attach cloud subtitles!
    let unrelated = plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(300)),
                    uri: Some("file:///Users/demo/LocalMovie.mkv".into()),
                    ..Default::default()
                },
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(unrelated.is_empty());
    assert!(
        host.tasks.is_empty(),
        "local file MediaOpened must not trigger BaiduPan subtitle requests"
    );
    assert!(plugin.windows[&Id(1)].last_resolved_video.is_none());

    // Opening the BaiduPan item with percent-encoded `MediaUri::Plugin` string matches and attaches subtitles.
    let encoded_uri = "iris-plugin://community.baidupan/files/bd%3A11%3A%2FMovies%2FSci%20Fi%2F%E7%AC%AC1%E8%AF%9D%3A%20%E5%90%AF%E7%A8%8B.mkv";
    plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(301)),
                    uri: Some(encoded_uri.into()),
                    ..Default::default()
                },
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (sub_op, _) = host.pop();
    let sub_cmds = feed_http(
        &mut plugin,
        &mut host,
        sub_op,
        200,
        "https://pcs.baidu.com/rest/2.0/pcs/file",
        vec![],
        br#"{"errno":0,"urls":[{"url":"https://qdall01.baidupcs.com/file/ep1.srt","encrypt":0}]}"#,
    );
    assert!(matches!(
        sub_cmds.as_slice(),
        [HostCommand::AttachSubtitle {
            format: SubtitleFormat::SubRip,
            lang: Some(lang),
            ..
        }] if lang == "zh-CN"
    ));
}

#[test]
fn secret_writes_close_superseded_handles_and_resubmit_across_media_generation_advance() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession::parse_openapi_input("expired", "rt", "cid", "sec").unwrap(),
        ..Default::default()
    };
    let mut host = MockHost::default();

    // Queue a background OAuth refresh SecretWrite, then supersede it with logout -> background op closed.
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(1),
                source_id: "files".into(),
                folder_id: Some("/".into()),
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (b1, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        b1,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        br#"{"errno":111}"#,
    );
    let (ref_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        ref_op,
        200,
        "https://openapi.baidu.com/oauth/2.0/token",
        vec![],
        br#"{"access_token":"NEW_AT","refresh_token":"NEW_RT"}"#,
    );
    let (bg_sec_op, bg_task) = host.pop();
    assert!(matches!(bg_task, Task::SecretWrite { value: Some(_), .. }));
    let (_retry_browse_op, _) = host.pop();

    // Logout closes `bg_sec_op` and submits tracked `logout_secret_op`.
    plugin
        .event(
            win(HostEvent::Action {
                id: "logout".into(),
                value: None,
                snapshot: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(host.closed.contains(&bg_sec_op));
    let (logout_op, logout_task) = host.pop();
    assert!(matches!(logout_task, Task::SecretWrite { value: None, .. }));

    // Advancing media generation before `logout_op` completes re-submits `SecretWrite(None)` without closing `logout_op`.
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
            1_700_000_000_000,
        )
        .unwrap();
    assert!(!host.closed.contains(&logout_op));
    let (_retry_logout_op, retry_logout_task) = host.pop();
    assert!(matches!(
        retry_logout_task,
        Task::SecretWrite { value: None, .. }
    ));
}
