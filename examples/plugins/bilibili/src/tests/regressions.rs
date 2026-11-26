use super::*;
use iris_plugin_sdk::api::v2::source::SourcePage;

#[test]
fn capped_dash_variants_include_the_preferred_or_explicit_selection() {
    let videos: Vec<_> = [127, 126, 125, 120, 116, 112, 80, 64]
        .into_iter()
        .flat_map(|qn| {
            [7, 12, 13].into_iter().map(move |codec| json!({
            "id":qn,"codecid":codec,"baseUrl":format!("https://upos.example.com/{qn}/{codec}.m4s")
        }))
        })
        .collect();
    let meta = playback::VideoMeta {
        bvid: "BV1xx411c7mD".into(),
        aid: 1,
        cid: 1,
        ep_id: None,
        title: "test".into(),
        source_id: "recommend".into(),
        entry_id: "ugc:BV1xx411c7mD:1".into(),
        playlist: vec![],
    };
    for (preferred_qn, requested, expected) in [
        (64, None, "v.64.hevc"),
        (127, None, "v.127.hevc"),
        (127, Some("v.64.av1"), "v.64.av1"),
    ] {
        let prefs = Preferences {
            preferred_qn,
            ..Default::default()
        };
        let media = playback::build_vod_media(
            &json!({"code":0,"data":{"dash":{"video":videos}}}),
            &meta,
            requested,
            &prefs,
            &StoredSession::default(),
        )
        .unwrap();
        assert_eq!(media.stream.active_variant.as_deref(), Some(expected));
        assert_eq!(media.stream.variants.len(), 16);
        assert!(media.stream.variants.iter().any(|v| v.id == expected));
        let ids: BTreeSet<_> = media.stream.variants.iter().map(|v| &v.id).collect();
        assert_eq!(ids.len(), 16);
    }
}

fn request(
    id: u64,
    source: &str,
    folder: Option<&str>,
    cursor: Option<String>,
    filters: &BTreeMap<String, String>,
) -> HostEvent<Value> {
    win(HostEvent::SourceBrowse {
        request_id: Id(id),
        source_id: source.into(),
        folder_id: folder.map(str::to_owned),
        page: cursor,
        query: None,
        filters: filters.clone(),
    })
}

fn page(commands: Vec<HostCommand>) -> SourcePage {
    assert!(serde_json::to_vec(&commands).unwrap().len() < 64 * 1024);
    let [command]: [HostCommand; 1] = commands.try_into().unwrap();
    let HostCommand::ProvideSourcePage { page, .. } = command else {
        panic!("expected a page")
    };
    *page
}

#[test]
fn complete_catalogs_paginate_without_losing_parts_episodes_folders_or_rankings() {
    let parts: Vec<_> = (1..=131)
        .map(|n| json!({"cid":n,"page":n,"part":format!("part {n}")}))
        .collect();
    let episodes: Vec<_> = (1..=131)
        .map(|n| json!({"id":n,"title":n.to_string()}))
        .collect();
    let folders: Vec<_> = (1..=131)
        .map(|n| json!({"id":n,"title":format!("folder {n}")}))
        .collect();
    let ranked: Vec<_> = (1..=131)
        .map(|n| json!({"bvid":format!("BV{n}"),"title":format!("video {n}")}))
        .collect();
    for (source, folder, filters, data) in [
        (
            "recommend",
            Some("ugc:BV1xx411c7mD"),
            BTreeMap::new(),
            json!({"pages":parts}),
        ),
        (
            "bangumi",
            Some("ss:1"),
            BTreeMap::new(),
            json!({"episodes":episodes}),
        ),
        ("favorites", None, BTreeMap::new(), json!({"list":folders})),
        (
            "popular",
            None,
            BTreeMap::from([("tab".into(), "rank_0".into())]),
            json!({"list":ranked}),
        ),
    ] {
        let mut plugin = BilibiliPlugin {
            session: StoredSession {
                sessdata: "fixture".into(),
                dede_user_id: "1".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut host = MockHost::default();
        plugin
            .event(
                request(100, source, folder, None, &filters),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap();
        let (op, _) = host.pop();
        let mut current = page(feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            vec![],
            &serde_json::to_vec(&json!({"code":0,"data":data})).unwrap(),
        ));
        let mut entries = Vec::new();
        let mut counts = Vec::new();
        let mut cursors = BTreeSet::new();
        loop {
            assert_eq!(current.folder_id.as_deref(), folder);
            counts.push(current.entries.len());
            entries.extend(current.entries.into_iter().map(|entry| entry.id));
            let Some(cursor) = current.next_page else {
                break;
            };
            assert!(cursors.insert(cursor.clone()), "pagination loop");
            current = page(
                plugin
                    .event(
                        request(
                            100 + cursors.len() as u64,
                            source,
                            folder,
                            Some(cursor),
                            &filters,
                        ),
                        &mut host,
                        1_700_000_000_000,
                    )
                    .unwrap(),
            );
        }
        assert_eq!(counts, [60, 60, 11]);
        assert_eq!(entries.len(), 131);
        assert_eq!(entries.iter().collect::<BTreeSet<_>>().len(), 131);
        assert!(
            host.tasks.is_empty(),
            "local pages must not refetch upstream"
        );
    }
}

#[test]
fn byte_limited_pages_preserve_remote_continuation_and_reject_stale_cursors() {
    let videos: Vec<_> = (1..=60)
        .map(|n| {
            json!({
                "bvid":format!("BV{n}"),"title":format!("video {n}"),
                "pic":format!("https://i0.hdslb.com/{}.jpg", "x".repeat(1600)),
            })
        })
        .collect();
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    let filters = BTreeMap::new();
    plugin
        .event(
            request(1, "popular", None, None, &filters),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (op, _) = host.pop();
    let mut current = page(feed_http(
        &mut plugin,
        &mut host,
        op,
        200,
        vec![],
        &serde_json::to_vec(&json!({"code":0,"data":{"list":videos,"no_more":false}})).unwrap(),
    ));
    assert!(!current.entries.is_empty() && current.entries.len() < 60);
    let first_cursor = current.next_page.clone().unwrap();
    let wrong_folder = plugin
        .event(
            request(
                2,
                "popular",
                Some("ugc:another"),
                Some(first_cursor.clone()),
                &filters,
            ),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        wrong_folder.as_slice(),
        [HostCommand::SourceError { .. }]
    ));
    let mut count = current.entries.len();
    let mut request_id = 3;
    while let Some(cursor) = current
        .next_page
        .as_deref()
        .filter(|c| c.starts_with("local:"))
    {
        current = page(
            plugin
                .event(
                    request(request_id, "popular", None, Some(cursor.into()), &filters),
                    &mut host,
                    1_700_000_000_000,
                )
                .unwrap(),
        );
        count += current.entries.len();
        request_id += 1;
    }
    assert_eq!(count, 60);
    assert_eq!(current.next_page.as_deref(), Some("2"));
    let replay = plugin
        .event(
            request(request_id, "popular", None, Some(first_cursor), &filters),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        replay.as_slice(),
        [HostCommand::SourceError { .. }]
    ));
    assert!(host.tasks.is_empty());
    plugin
        .event(
            request(request_id + 1, "popular", None, current.next_page, &filters),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (_, task) = host.pop();
    assert!(matches!(task, Task::HttpStart { url, .. } if url.contains("pn=2")));
}

#[test]
fn local_continuation_is_bound_to_the_window_context_and_current_listing() {
    for boundary in ["window", "context", "listing", "query", "filters", "media"] {
        let mut plugin = BilibiliPlugin::default();
        let mut host = MockHost::default();
        let filters = BTreeMap::new();
        plugin
            .event(
                request(1, "recommend", Some("ugc:BV1"), None, &filters),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap();
        let (op, _) = host.pop();
        let parts: Vec<_> = (1..=61)
            .map(|n| json!({"cid":n,"page":n,"part":format!("part {n}")}))
            .collect();
        let first = page(feed_http(
            &mut plugin,
            &mut host,
            op,
            200,
            vec![],
            &serde_json::to_vec(&json!({"code":0,"data":{"pages":parts}})).unwrap(),
        ));
        if boundary == "listing" {
            plugin
                .event(
                    request(2, "recommend", Some("ugc:BV2"), None, &filters),
                    &mut host,
                    1_700_000_000_000,
                )
                .unwrap();
            host.pop();
        }
        let mut event = request(3, "recommend", Some("ugc:BV1"), first.next_page, &filters);
        let HostEvent::Window {
            window,
            generation,
            body,
        } = &mut event
        else {
            unreachable!()
        };
        match boundary {
            "window" => *window = Id(2),
            "context" => generation.context.0 += 1,
            "media" => generation.media.0 += 1,
            "query" | "filters" => {
                let HostEvent::SourceBrowse { query, filters, .. } = body.as_mut() else {
                    unreachable!()
                };
                if boundary == "query" {
                    *query = Some("another query".into());
                } else {
                    filters.insert("tab".into(), "another tab".into());
                }
            }
            _ => {}
        }
        let commands = plugin.event(event, &mut host, 1_700_000_000_000).unwrap();
        if boundary == "media" {
            // Starting playback must not invalidate the still-open source view.
            assert_eq!(page(commands).entries.len(), 1);
        } else {
            assert!(
                matches!(commands.as_slice(), [HostCommand::SourceError { .. }]),
                "{boundary}"
            );
        }
        assert!(host.tasks.is_empty(), "{boundary}");
    }
}

#[test]
fn exhausted_local_browse_page_clears_cache_state() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    let filters = BTreeMap::new();
    plugin
        .event(
            request(1, "recommend", Some("ugc:BV1"), None, &filters),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (op, _) = host.pop();
    let parts: Vec<_> = (1..=61)
        .map(|n| json!({"cid":n,"page":n,"part":format!("part {n}")}))
        .collect();
    let first = page(feed_http(
        &mut plugin,
        &mut host,
        op,
        200,
        vec![],
        &serde_json::to_vec(&json!({"code":0,"data":{"pages":parts}})).unwrap(),
    ));
    assert!(plugin.windows[&Id(1)].browse_page.is_some());
    let last = page(
        plugin
            .event(
                request(2, "recommend", Some("ugc:BV1"), first.next_page, &filters),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap(),
    );
    assert_eq!(last.entries.len(), 1);
    assert!(last.next_page.is_none());
    assert!(
        plugin.windows[&Id(1)].browse_page.is_none(),
        "exhausted local PageCache must be cleared from WindowState"
    );
}

#[test]
fn generation_change_discards_scoped_state_without_closing_revoked_handles() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    plugin
        .event(
            request(1, "popular", None, None, &BTreeMap::new()),
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
fn browse_403_waits_for_nav_before_retrying_with_fresh_wbi_signature() {
    let mut plugin = BilibiliPlugin {
        wbi: auth::WbiKeys {
            img_key: "00000000000000000000000000000000".into(),
            sub_key: "11111111111111111111111111111111".into(),
            updated_secs: 1_700_000_000,
        },
        ..Default::default()
    };
    let mut host = MockHost::default();
    plugin
        .event(
            request(55, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (browse_op, task) = host.pop();
    assert!(matches!(task, Task::HttpStart { url, .. } if url.contains("w_rid=")));

    // -403 must trigger ONLY Nav and hold the browse retry until Nav completes.
    let cmds = feed_http(
        &mut plugin,
        &mut host,
        browse_op,
        200,
        vec![],
        br#"{"code":-403,"message":"access denied"}"#,
    );
    assert!(cmds.is_empty());
    assert_eq!(
        host.tasks.len(),
        1,
        "must not dispatch unsigned browse concurrently with Nav"
    );
    let (nav_op, nav_task) = host.pop();
    assert!(
        matches!(nav_task, Task::HttpStart { url, .. } if url.contains("/x/web-interface/nav"))
    );
    assert_eq!(plugin.windows[&Id(1)].pending_wbi_browse.len(), 1);

    // Once Nav completes with fresh WBI keys, the queued browse retries WITH a valid signature.
    let _ = feed_http(
        &mut plugin,
        &mut host,
        nav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":false,"wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );
    assert!(plugin.windows[&Id(1)].pending_wbi_browse.is_empty());
    let (retry_op, retry_task) = host.pop();
    assert!(
        matches!(retry_task, Task::HttpStart { url, .. } if url.contains("wbi/index/top/feed/rcmd") && url.contains("w_rid=")),
        "retried browse must be WBI-signed after Nav returns"
    );
    let retry_cmds = feed_http(
        &mut plugin,
        &mut host,
        retry_op,
        200,
        vec![],
        br#"{"code":0,"data":{"item":[{"bvid":"BV1ok","title":"Recovered"}]}}"#,
    );
    let p = page(retry_cmds);
    assert_eq!(p.entries.len(), 1);
    assert_eq!(p.entries[0].id, "BV1ok");

    // If Nav fails while a browse retry is queued, the request receives SourceError instead of hanging.
    plugin
        .event(
            request(56, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (b2_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        b2_op,
        200,
        vec![],
        br#"{"code":-403,"message":"access denied"}"#,
    );
    let (nav2_op, _) = host.pop();
    let err_cmds = plugin
        .event(
            win(done(
                nav2_op,
                json!({"error":{"code": ErrorCode::Timeout as i32, "message":"Timeout"}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        err_cmds.as_slice(),
        [HostCommand::SourceError {
            request_id: Id(56),
            ..
        }]
    ));
}

#[test]
fn dash_preserves_hires_and_dolby_audio_even_with_three_aac_tracks() {
    let meta = playback::VideoMeta {
        bvid: "BV1audio".into(),
        aid: 1,
        cid: 1,
        ep_id: None,
        title: "4K Audio Test".into(),
        source_id: "recommend".into(),
        entry_id: "ugc:BV1audio:1".into(),
        playlist: vec![],
    };
    let json = json!({
        "code": 0,
        "data": {
            "dash": {
                "video": [{
                    "id": 120,
                    "codecid": 12,
                    "codecs": "hev1",
                    "bandwidth": 8_000_000,
                    "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/v.m4s"
                }],
                "audio": [
                    {"id": 30280, "bandwidth": 192_000, "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/aac192.m4s"},
                    {"id": 30232, "bandwidth": 132_000, "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/aac132.m4s"},
                    {"id": 30216, "bandwidth": 64_000,  "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/aac64.m4s"}
                ],
                "flac": {
                    "audio": {"id": 30251, "bandwidth": 1_500_000, "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/flac.m4s"}
                },
                "dolby": {
                    "audio": [{"id": 30250, "bandwidth": 384_000, "baseUrl": "https://upos-sz-mirrorcos.bilivideo.com/dolby.m4s"}]
                }
            }
        }
    });
    let media = playback::build_vod_media(
        &json,
        &meta,
        None,
        &Preferences::default(),
        &StoredSession::default(),
    )
    .unwrap();
    assert_eq!(media.stream.audio_tracks.len(), 3);
    assert!(media.stream.audio_tracks[0].default);
    assert!(
        media.stream.audio_tracks[0]
            .resource
            .url
            .ends_with("/aac192.m4s")
    );
    assert_eq!(media.stream.audio_tracks[1].label, "Hi-Res 无损");
    assert!(
        media.stream.audio_tracks[1]
            .resource
            .url
            .ends_with("/flac.m4s")
    );
    assert_eq!(media.stream.audio_tracks[2].label, "杜比音效");
    assert!(
        media.stream.audio_tracks[2]
            .resource
            .url
            .ends_with("/dolby.m4s")
    );
}

#[test]
fn cookie_and_qr_parsing_split_newlines_and_sanitize_control_chars() {
    let multiline =
        "SESSDATA=abc\x07def\r\nbili_jct=csrf\x00token\nDedeUserID=42\rbuvid3=dev\x1Fid";
    let sess = StoredSession::parse_cookie_input(multiline, "fallback").unwrap();
    assert_eq!(sess.sessdata, "abcdef");
    assert_eq!(sess.bili_jct, "csrftoken");
    assert_eq!(sess.dede_user_id, "42");
    assert_eq!(sess.buvid3, "devid");

    let bare = StoredSession::parse_cookie_input("bare\r\ntoken\x07", "dev\r\n1").unwrap();
    assert_eq!(bare.sessdata, "baretoken");
    assert_eq!(bare.buvid3, "dev1");

    let qr = auth::parse_qr_poll(
        &json!({"code":0,"data":{"code":0,"url":"https://passport.biligame.com/x?DedeUserID=99"}}),
        &[("Set-Cookie".into(), "SESSDATA=qr\x07sess; Path=/".into())],
        "dev\n2",
    )
    .unwrap();
    match qr {
        auth::QrPollOutcome::Success(s) => {
            assert_eq!(s.sessdata, "qrsess");
            assert_eq!(s.buvid3, "dev2");
        }
        _ => panic!("expected Success"),
    }
}

#[test]
fn zero_clock_rejects_wbi_signing_and_logs_instead_of_clamping_to_fake_timestamp() {
    let mut plugin = BilibiliPlugin {
        wbi: auth::WbiKeys {
            img_key: "7cd084941338484aae1ad9425b84077c".into(),
            sub_key: "4932caff0ff746eab6f01bf08b70ac45".into(),
            updated_secs: 1_700_000_000,
        },
        ..Default::default()
    };
    assert!(!plugin.wbi.is_fresh(0));
    let mut host = MockHost::default();
    let cmds = plugin
        .event(
            request(91, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            0,
        )
        .unwrap();
    assert!(
        host.tasks.is_empty(),
        "must not WBI-sign with fake 2023 timestamp"
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
fn cold_start_wbi_browse_and_playurl_wait_for_nav_and_stale_403_preserves_refreshed_keys() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();

    // 1. Cold-start WBI browse queues behind Nav without firing an unsigned wbi/ request.
    let cmds = plugin
        .event(
            request(701, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(cmds.is_empty());
    assert_eq!(host.tasks.len(), 1);
    let (nav_op, nav_task) = host.pop();
    assert!(
        matches!(nav_task, Task::HttpStart { url, .. } if url.contains("/x/web-interface/nav"))
    );

    // 2. Cold-start UGC resolve runs view, then queues playurl behind the SAME in-flight Nav.
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(702),
                source_id: "recommend".into(),
                folder_id: None,
                entry_id: "BV1uv411q7Mv".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (view_op, _) = host.pop();
    let view_cmds = feed_http(
        &mut plugin,
        &mut host,
        view_op,
        200,
        vec![],
        br#"{"code":0,"data":{"bvid":"BV1uv411q7Mv","aid":100,"cid":555,"title":"ColdStart","pages":[{"cid":555,"page":1,"part":"P1"}]}}"#,
    );
    assert!(view_cmds.is_empty());
    assert!(
        host.tasks.is_empty(),
        "cold-start UGC playurl must wait for in-flight Nav instead of sending unsigned query"
    );

    // 3. Completing Nav dispatches BOTH queued WBI requests with w_rid signatures.
    let _ = feed_http(
        &mut plugin,
        &mut host,
        nav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":false,"wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );
    assert_eq!(host.tasks.len(), 2);
    let (_b_retry_op, b_retry_task) = host.pop();
    let (p_retry_op, p_retry_task) = host.pop();
    assert!(
        matches!(b_retry_task, Task::HttpStart { url, .. } if url.contains("wbi/index/top/feed/rcmd") && url.contains("w_rid="))
    );
    assert!(
        matches!(p_retry_task, Task::HttpStart { url, .. } if url.contains("/x/player/wbi/playurl") && url.contains("w_rid="))
    );
    let resolved = feed_http(
        &mut plugin,
        &mut host,
        p_retry_op,
        200,
        vec![],
        br#"{"code":0,"data":{"dash":{"duration":30,"video":[{"id":80,"codecid":7,"codecs":"avc1","bandwidth":1000000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/v.m4s"}],"audio":[{"id":30280,"bandwidth":128000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/a.m4s"}]}}}"#,
    );
    assert!(matches!(
        resolved.as_slice(),
        [HostCommand::ResolveStream {
            request_id: Id(702),
            ..
        }]
    ));

    // 4. Two concurrent WBI requests sent with stale keys: second -403 after Nav refresh must NOT wipe fresh keys.
    plugin.wbi = auth::WbiKeys {
        img_key: "00000000000000000000000000000000".into(),
        sub_key: "11111111111111111111111111111111".into(),
        updated_secs: 1_700_000_000,
    };
    plugin
        .event(
            request(703, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (stale_op1, _) = host.pop();
    plugin
        .event(
            request(704, "recommend", None, None, &BTreeMap::new()),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (stale_op2, _) = host.pop();

    // First stale -403 triggers Nav; Nav refreshes WBI keys and retries 703.
    feed_http(
        &mut plugin,
        &mut host,
        stale_op1,
        200,
        vec![],
        br#"{"code":-403,"message":"stale"}"#,
    );
    let (nav2_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        nav2_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":false,"wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );
    let (_retry_703_op, _) = host.pop();
    let refreshed_mixin = plugin.wbi.mixin().expect("fresh mixin must exist");

    // Second stale -403 arrives AFTER refresh: must keep refreshed_mixin and immediately retry 704 signed!
    feed_http(
        &mut plugin,
        &mut host,
        stale_op2,
        200,
        vec![],
        br#"{"code":-403,"message":"stale"}"#,
    );
    assert_eq!(
        plugin.wbi.mixin().as_deref(),
        Some(refreshed_mixin.as_str())
    );
    assert_eq!(host.tasks.len(), 1);
    let (_, retry_704_task) = host.pop();
    assert!(
        matches!(retry_704_task, Task::HttpStart { url, .. } if url.contains("wbi/index/top/feed/rcmd") && url.contains("w_rid="))
    );
}
