use super::*;
use iris_plugin_sdk::api::v2::{Role, stream::StreamSource};
use serde_json::json;
use std::collections::VecDeque;

mod account_lifecycle;
mod regressions;

impl crate::enhance::CidCache {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Default)]
struct MockHost {
    next: u64,
    tasks: VecDeque<(Id, Task)>,
    blobs: BTreeMap<Id, Vec<u8>>,
    closed: Vec<Id>,
}

impl MockHost {
    fn pop(&mut self) -> (Id, Task) {
        self.tasks.pop_front().expect("expected submitted task")
    }
}

impl Host for MockHost {
    fn submit(&mut self, task: &Task) -> Result<Id, ErrorCode> {
        self.next += 1;
        let id = Id(self.next);
        self.tasks.push_back((id, task.clone()));
        Ok(id)
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<Id, ErrorCode> {
        self.next += 1;
        let id = Id(self.next);
        self.blobs.insert(id, bytes.to_vec());
        Ok(id)
    }
    fn take_blob(&mut self, id: Id) -> Result<Vec<u8>, ErrorCode> {
        self.blobs.remove(&id).ok_or(ErrorCode::StaleHandle)
    }
    fn close(&mut self, id: Id) {
        self.closed.push(id);
    }
    fn random(&mut self, bytes: &mut [u8]) -> Result<(), ErrorCode> {
        bytes.fill(0xAB);
        Ok(())
    }
}

fn win(event: HostEvent<Value>) -> HostEvent<Value> {
    HostEvent::Window {
        window: Id(1),
        generation: Generation::default(),
        body: Box::new(event),
    }
}

fn done(op: Id, val: Value) -> HostEvent<Value> {
    HostEvent::Completed {
        operation_id: op,
        generation: Generation::default(),
        result: val,
    }
}

fn feed_http(
    plugin: &mut BilibiliPlugin,
    host: &mut MockHost,
    op: Id,
    status: u16,
    headers: Vec<(String, String)>,
    body: &[u8],
) -> Vec<HostCommand> {
    plugin
        .event(
            win(done(
                op,
                json!({
                    "ok": {
                        "state": "response",
                        "stream_handle": "900",
                        "status": status,
                        "url": "https://api.bilibili.com/ok",
                        "headers": headers,
                        "written": null
                    }
                }),
            )),
            host,
            1_700_000_000_000,
        )
        .unwrap();
    let (read_op, task) = host.pop();
    assert!(matches!(task, Task::HttpRead { .. }));
    let blob_id = host.blob(body).unwrap();
    plugin
        .event(
            win(done(
                read_op,
                json!({"ok": {"eof": true, "text": null, "blob": blob_id.0.to_string()}}),
            )),
            host,
            1_700_000_000_000,
        )
        .unwrap()
}

#[test]
fn wbi_md5_and_target_parsing_match_bilibili_spec() {
    assert_eq!(wbi::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(wbi::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    let img =
        wbi::extract_wbi_stem("https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png")
            .unwrap();
    let sub =
        wbi::extract_wbi_stem("https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png")
            .unwrap();
    let mixin = wbi::mixin_key(&img, &sub).unwrap();
    assert_eq!(mixin, "ea1db124af3c7062474693fa704f4ff8");
    let signed = wbi::sign_wbi_query(
        &[
            ("foo", "114".into()),
            ("bar", "514".into()),
            ("zab", "1919810".into()),
        ],
        &mixin,
        1_702_204_169,
    );
    assert!(signed.contains("w_rid=8f6f2b5b3d485fe1886cec6a0be8c5d4"));

    assert_eq!(
        wbi::parse_target("https://www.bilibili.com/video/BV1uv411q7Mv/?p=3"),
        Some(BiliTarget::Bvid {
            bvid: "BV1uv411q7Mv".into(),
            cid: None,
            page: 3
        })
    );
    assert_eq!(
        wbi::parse_target("https://www.bilibili.com/bangumi/play/ep754321"),
        Some(BiliTarget::Episode(754321))
    );
    assert_eq!(
        wbi::parse_target("https://live.bilibili.com/21452505"),
        Some(BiliTarget::LiveRoom(21452505))
    );
}

#[test]
fn qr_login_dash_upos_selection_danmaku_and_bcc_subtitles_work_end_to_end() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    plugin
        .event(
            HostEvent::Init {
                protocol: 2,
                abi: 2,
                host_version: "0.0.1".into(),
                role: Role::Control,
                generation: Generation::default(),
                capabilities: Default::default(),
                features: Default::default(),
                snapshot: None,
            },
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let _ = host.pop(); // SecretRead
    let _ = host.pop(); // StateRead

    let cmds = plugin
        .event(
            win(HostEvent::WindowOpened { snapshot: None }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, HostCommand::SetSidebar { .. }))
    );
    let (nav_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        nav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":false,"wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );

    // 1. QR login flow (including silent auto-poll + automatic post-login source refresh)
    plugin
        .event(
            win(HostEvent::Action {
                id: "start_qr".into(),
                value: None,
                snapshot: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (qr_op, _) = host.pop();
    let qr_cmds = feed_http(
        &mut plugin,
        &mut host,
        qr_op,
        200,
        vec![],
        br#"{"code":0,"data":{"url":"https://passport.bilibili.com/h5-app/passport/login/scan?qrcode_key=abc123456","qrcode_key":"abc123456"}}"#,
    );
    assert!(plugin.windows[&Id(1)].qr.as_ref().unwrap().matrix.len() >= 29);
    let has_auto_poll = qr_cmds.iter().any(|c| match c {
        HostCommand::SetSidebar { panel: Some(p) } => {
            p.timer
                .as_ref()
                .is_some_and(|t| t.action_id == "auto_poll_qr" && t.interval_ms == 2_000)
                && p.sections.iter().any(|s| {
                    s.widgets
                        .iter()
                        .any(|w| matches!(w, iris_plugin_sdk::api::Widget::PixelGrid { .. }))
                })
        }
        _ => false,
    });
    assert!(has_auto_poll);

    plugin
        .event(
            win(HostEvent::Action {
                id: "auto_poll_qr".into(),
                value: None,
                snapshot: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (wait_op, _) = host.pop();
    let wait_cmds = feed_http(
        &mut plugin,
        &mut host,
        wait_op,
        200,
        vec![],
        br#"{"code":0,"data":{"code":86090,"message":"scanned"}}"#,
    );
    assert!(
        !wait_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::ShowToast { .. }))
    );

    plugin
        .event(
            win(HostEvent::Action {
                id: "auto_poll_qr".into(),
                value: None,
                snapshot: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (poll_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        poll_op,
        200,
        vec![(
            "Set-Cookie".into(),
            "SESSDATA=secret_sess; Path=/; HttpOnly".into(),
        )],
        br#"{"code":0,"data":{"code":0,"url":"https://passport.biligame.com/x?DedeUserID=9988&bili_jct=csrf1"}}"#,
    );
    let (sec_op, sec_task) = host.pop();
    assert!(matches!(sec_task, Task::SecretWrite { .. }));
    let login_cmds = plugin
        .event(
            win(done(
                sec_op,
                json!({"ok": {"stored": true, "revision": null}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(
        login_cmds.iter().any(
            |c| matches!(c, HostCommand::RefreshSource { source_id } if source_id == "dynamic")
        )
    );
    let (nav2_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        nav2_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":true,"uname":"SailUser"}}"#,
    );
    assert_eq!(plugin.session.uname.as_deref(), Some("SailUser"));

    // 2. Resolve BV -> split-track DirectHttp DASH (.m4s) with upos mirror preferred over PCDN/MCDN
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(77),
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
    feed_http(
        &mut plugin,
        &mut host,
        view_op,
        200,
        vec![],
        br#"{"code":0,"data":{"bvid":"BV1uv411q7Mv","aid":100,"cid":555,"title":"Demo","pages":[{"cid":555,"page":1,"part":"P1"}]}}"#,
    );
    let (play_op, _) = host.pop();
    let resolved = feed_http(
        &mut plugin,
        &mut host,
        play_op,
        200,
        vec![],
        br#"{"code":0,"data":{"dash":{"duration":120,"video":[{"id":80,"codecid":12,"codecs":"hev1.1.6.L120.90","bandwidth":2000000,"width":1920,"height":1080,"baseUrl":"https://x5zy69j.edge.mountaintoys.cn:4483/v.m4s","backupUrl":["https://xy.mcdn.bilivideo.cn:4480/v.m4s","https://upos-sz-mirrorcos.bilivideo.com/v.m4s"],"SegmentBase":{"Initialization":"0-999","indexRange":"1000-1499"}}],"audio":[{"id":30280,"codecs":"mp4a.40.2","bandwidth":192000,"baseUrl":"https://x5zy69j.edge.mountaintoys.cn:4483/a.m4s","backupUrl":["https://upos-sz-mirrorcos.bilivideo.com/a.m4s"],"SegmentBase":{"Initialization":"0-599","indexRange":"600-999"}}]}}}"#,
    );
    let [HostCommand::ResolveStream { media, .. }] = resolved.as_slice() else {
        panic!("expected ResolveStream");
    };
    let StreamSource::DirectHttp {
        primary,
        fallback_urls,
        identity,
    } = &media.stream.source
    else {
        panic!("expected DirectHttp split-track video .m4s");
    };
    assert!(fallback_urls.is_empty() && identity.is_none());
    assert!(
        primary
            .url
            .starts_with("https://upos-sz-mirrorcos.bilivideo.com/v.m4s")
    );
    assert_eq!(media.stream.audio_tracks.len(), 1);
    assert!(media.stream.audio_tracks[0].default);
    assert!(
        media.stream.audio_tracks[0]
            .resource
            .url
            .starts_with("https://upos-sz-mirrorcos.bilivideo.com/a.m4s")
    );

    // 3. Danmaku window slicing + BCC subtitle -> WebVTT conversion
    let items = danmaku::parse_danmaku_xml(
        r#"<i><d p="1.5,1,25,16777215,0,0,0,1">前方高能</d><d p="118.0,1,25,65280,0,0,0,2">结尾弹幕</d></i>"#,
    );
    let win_spec = OverlayWindow {
        revision: Id(1),
        start_ms: Id(0),
        end_ms: Id(120_000),
    };
    let cues = danmaku::cues_for_window(&items, &win_spec, 80);
    assert_eq!(cues.len(), 2);
    assert!(
        cues.iter()
            .all(|c| c.start_ms.0 + c.duration_ms.0 <= 120_000)
    );

    let vtt = danmaku::bcc_json_to_webvtt(
        r#"{"body":[{"from":0.5,"to":2.25,"content":"你好，世界"}]}"#.as_bytes(),
    )
    .unwrap();
    assert!(vtt.starts_with("WEBVTT\n\n00:00:00.500 --> 00:00:02.250\n你好，世界"));
}

#[test]
fn raw_deflate_danmaku_denied_hint_and_direct_http_variants_comply_with_host_rules() {
    let xml = r#"<i><d p="2.0,1,25,16777215,0,0,0,1">压缩弹幕</d></i>"#.as_bytes();
    let compressed = miniz_oxide::deflate::compress_to_vec(xml, 6);
    let decoded = danmaku::decode_http_body(
        &[("content-encoding".into(), "deflate".into())],
        &compressed,
        1024 * 1024,
    )
    .unwrap();
    assert_eq!(decoded, xml);
    assert_eq!(
        danmaku::parse_danmaku_xml(std::str::from_utf8(&decoded).unwrap()).len(),
        1
    );

    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    plugin
        .event(
            win(HostEvent::WindowOpened { snapshot: None }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let _ = host.pop(); // initial nav
    plugin
        .event(
            win(HostEvent::Action {
                id: "start_qr".into(),
                value: None,
                snapshot: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (qr_op, _) = host.pop();
    let denied = plugin
        .event(
            win(done(
                qr_op,
                json!({"error": {"code": ErrorCode::Denied as i32, "message": "Denied"}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        denied.as_slice(),
        [HostCommand::ShowToast { message }] if message.contains("Fake-IP")
    ));

    let live_json: Value = serde_json::from_str(
        r#"{"code":0,"data":{"live_status":1,"playurl_info":{"playurl":{"stream":[{"protocol_name":"http_hls","format":[{"codec":[{"base_url":"/live.m3u8","url_info":[{"host":"https://cn-hbwh.bilivideo.com","extra":"?a=1"},{"host":"https://upos-sz-live.bilivideo.com","extra":"?b=2"}]}]}]}]}}}}"#,
    )
    .unwrap();
    let live = playback::build_live_media(
        &live_json,
        6,
        Some("hls.1"),
        None,
        "live",
        &StoredSession::default(),
    )
    .unwrap();
    let StreamSource::DirectHttp {
        primary,
        fallback_urls,
        identity,
    } = &live.stream.source
    else {
        panic!("expected DirectHttp");
    };
    assert!(fallback_urls.is_empty() && identity.is_none());
    assert!(primary.url.starts_with("https://cn-hbwh.bilivideo.com/"));
    assert_eq!(live.stream.variants.len(), 2);
}

#[test]
fn browse_deduplication_text_sanitization_and_allowed_port_ranking() {
    // 1. Control & bidi chars are stripped from titles/subtitles so host validation never quarantines.
    let clean = auth::sanitize_text("热门\u{202E}视频\n\t测试  标题", 240);
    assert_eq!(clean, "热门视频 测试 标题");
    assert!(!clean.chars().any(char::is_control));

    // 2. Paginated browse deduplicates overlapping entries across pages and within a page.
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();
    plugin
        .event(
            win(HostEvent::WindowOpened { snapshot: None }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let _ = host.pop(); // nav
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(10),
                source_id: "popular".into(),
                folder_id: None,
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (p1_op, _) = host.pop();
    let p1 = feed_http(
        &mut plugin,
        &mut host,
        p1_op,
        200,
        vec![],
        br#"{"code":0,"data":{"list":[{"bvid":"BV1A","title":"A\u202e\n1"},{"bvid":"BV1A","title":"dup"},{"bvid":"BV1B","title":"B"}],"no_more":false}}"#,
    );
    let [HostCommand::ProvideSourcePage { page: page1, .. }] = p1.as_slice() else {
        panic!("expected page 1");
    };
    assert_eq!(page1.entries.len(), 2);
    assert_eq!(page1.entries[0].id, "BV1A");
    assert_eq!(page1.entries[0].name, "A 1");
    assert_eq!(page1.next_page.as_deref(), Some("2"));

    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(11),
                source_id: "popular".into(),
                folder_id: None,
                page: Some("2".into()),
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (p2_op, _) = host.pop();
    let p2 = feed_http(
        &mut plugin,
        &mut host,
        p2_op,
        200,
        vec![],
        br#"{"code":0,"data":{"list":[{"bvid":"BV1B","title":"overlap"},{"bvid":"BV1C","title":"C"}],"no_more":true}}"#,
    );
    let [HostCommand::ProvideSourcePage { page: page2, .. }] = p2.as_slice() else {
        panic!("expected page 2");
    };
    assert_eq!(page2.entries.len(), 1);
    assert_eq!(page2.entries[0].id, "BV1C");

    // 3. Disallowed MCDN ports are skipped in favor of allowed ports, and AAC is default alongside Hi-Res.
    let meta = playback::VideoMeta {
        bvid: "BV1TEST".into(),
        aid: 1,
        cid: 2,
        ep_id: None,
        title: "Test\u{202E}\nTitle".into(),
        source_id: "popular".into(),
        entry_id: "BV1TEST".into(),
        playlist: Vec::new(),
    };
    let play_json: Value = serde_json::from_str(
        r#"{"code":0,"data":{"dash":{"video":[{"id":80,"codecid":12,"codecs":"hev1","bandwidth":2000000,"baseUrl":"https://edge.bilivideo.cn:18443/v.m4s","backupUrl":["https://xy.mcdn.bilivideo.cn:4480/v.m4s"]}],"flac":{"audio":{"bandwidth":1000000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/flac.m4s"}},"audio":[{"id":30280,"bandwidth":192000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/aac.m4s"}]}}}"#,
    )
    .unwrap();
    let media = playback::build_vod_media(
        &play_json,
        &meta,
        None,
        &Preferences::default(),
        &StoredSession::default(),
    )
    .unwrap();
    let StreamSource::DirectHttp { primary, .. } = &media.stream.source else {
        panic!("expected DirectHttp");
    };
    assert_eq!(primary.url, "https://xy.mcdn.bilivideo.cn:4480/v.m4s");
    assert_eq!(media.stream.title.as_deref(), Some("Test Title"));
    assert_eq!(media.stream.audio_tracks.len(), 2);
    assert!(
        media.stream.audio_tracks[0]
            .resource
            .url
            .ends_with("/aac.m4s")
    );
    assert!(media.stream.audio_tracks[0].default);
    assert!(
        media.stream.audio_tracks[1]
            .resource
            .url
            .ends_with("/flac.m4s")
    );
    assert!(!media.stream.audio_tracks[1].default);
}

#[test]
fn source_auth_state_and_bilibili_subtitles_work_across_multipage_and_ai_tracks() {
    let mut plugin = BilibiliPlugin::default();
    let mut host = MockHost::default();

    // 1. Init loads saved session -> SetSources immediately clears requires_auth on dynamic/favorites/history.
    plugin
        .event(
            HostEvent::Init {
                protocol: 2,
                abi: 2,
                host_version: "0.0.1".into(),
                role: Role::Control,
                generation: Generation::default(),
                capabilities: Default::default(),
                features: Default::default(),
                snapshot: None,
            },
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (sec_read_op, _) = host.pop();
    let (_prefs_read_op, _) = host.pop();
    let init_cmds = plugin
        .event(
            done(
                sec_read_op,
                json!({"ok": {"store_value": r#"{"sessdata":"saved_sess","bili_jct":"j","dede_user_id":"1","buvid3":"b"}"#}}),
            ),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let Some(HostCommand::SetSources { sources }) = init_cmds.first() else {
        panic!("expected SetSources on Init SecretRead completion");
    };
    assert!(sources.iter().all(|s| !s.requires_auth));

    // 2. Multi-page video resolved by BV matches MediaOpened and fetches subtitles without Cookie header.
    plugin
        .event(
            win(HostEvent::WindowOpened { snapshot: None }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (nav_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        nav_op,
        200,
        vec![],
        br#"{"code":0,"data":{"isLogin":true,"uname":"Tester","wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png","sub_url":"https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}}"#,
    );
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(88),
                source_id: "popular".into(),
                folder_id: None,
                entry_id: "BV1uv411q7Mv".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (view_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        view_op,
        200,
        vec![],
        br#"{"code":0,"data":{"bvid":"BV1uv411q7Mv","aid":200,"cid":777,"title":"Multi","pages":[{"cid":777,"page":1,"part":"P1"},{"cid":778,"page":2,"part":"P2"}]}}"#,
    );
    let (play_op, _) = host.pop();
    let _ = feed_http(
        &mut plugin,
        &mut host,
        play_op,
        200,
        vec![],
        br#"{"code":0,"data":{"dash":{"duration":60,"video":[{"id":80,"codecid":12,"codecs":"hev1","bandwidth":1000000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/v.m4s"}],"audio":[{"id":30280,"bandwidth":128000,"baseUrl":"https://upos-sz-mirrorcos.bilivideo.com/a.m4s"}]}}}"#,
    );
    plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(1)),
                    uri: Some("iris-plugin://community.bilibili/popular/BV1uv411q7Mv".into()),
                    ..Default::default()
                },
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (_dm_op, _) = host.pop();
    let (pv2_op, pv2_task) = host.pop();
    assert!(matches!(pv2_task, Task::HttpStart { url, .. } if url.contains("w_rid=")));
    let _ = feed_http(
        &mut plugin,
        &mut host,
        pv2_op,
        200,
        vec![],
        r#"{"code":0,"data":{"subtitle":{"subtitles":[{"lan":"en-US","lan_doc":"English","subtitle_url":"//aisubtitle.hdslb.com/en.json"},{"lan":"ai-zh","lan_doc":"中文（自动生成）","subtitle_url":"","subtitle_url_v2":"//aisubtitle.hdslb.com/ai-zh.json"}]}}}"#.as_bytes(),
    );
    // Chinese AI subtitle is prioritized ahead of English and fetched without Cookie headers.
    let (sub_op, sub_task) = host.pop();
    let Task::HttpStart { url, headers, .. } = sub_task else {
        panic!("expected SubtitleBcc HttpStart");
    };
    assert_eq!(url, "https://aisubtitle.hdslb.com/ai-zh.json");
    assert!(
        !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("cookie"))
    );
    let _ = feed_http(
        &mut plugin,
        &mut host,
        sub_op,
        200,
        vec![],
        r#"{"body":[{"from":null,"to":1,"content":"bad"},{"from":1.0,"to":3.5,"content":"第一行\r\n\r\n第二行"}]}"#.as_bytes(),
    );
    let (art_begin_op, _) = host.pop();
    let (_next_sub_op, _) = host.pop(); // en-US queued sequentially
    plugin
        .event(
            win(done(
                art_begin_op,
                json!({"ok": {"artifact_writer": "501"}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (art_write_op, art_write_task) = host.pop();
    let Task::ArtifactWrite { blob, .. } = art_write_task else {
        panic!("expected ArtifactWrite");
    };
    let vtt_text = String::from_utf8(host.blobs.get(&blob).unwrap().clone()).unwrap();
    assert!(vtt_text.contains("00:00:01.000 --> 00:00:03.500\n第一行\n第二行\n\n"));
    let attach_cmds = plugin
        .event(
            win(done(
                art_write_op,
                json!({"ok": {"artifact": "601", "bytes": "64", "format": "vtt"}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(matches!(
        attach_cmds.as_slice(),
        [HostCommand::AttachSubtitle { lang: Some(lang), .. }] if lang == "zh-CN"
    ));

    // 3. Logout restores requires_auth = true on dynamic/favorites/history.
    let logout_cmds = plugin
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
    let Some(HostCommand::SetSources { sources }) = logout_cmds
        .iter()
        .find(|c| matches!(c, HostCommand::SetSources { .. }))
    else {
        panic!("expected SetSources on logout");
    };
    assert!(
        sources
            .iter()
            .filter(|s| matches!(s.id.as_str(), "dynamic" | "favorites" | "history"))
            .all(|s| s.requires_auth)
    );
}
