use super::*;
use iris_plugin_sdk::api::{
    SourceEntryKind, Widget,
    v2::{
        Role,
        enhance::SubtitleFormat,
        stream::{ManifestFormat, StreamSource, VirtualHls},
    },
};
use serde_json::json;
use std::collections::VecDeque;

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
        bytes.fill(0x3C);
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
    plugin: &mut BaiduPanPlugin,
    host: &mut MockHost,
    op: Id,
    status: u16,
    final_url: &str,
    headers: Vec<(String, String)>,
    body: &[u8],
) -> Vec<HostCommand> {
    let cmds = plugin
        .event(
            win(done(
                op,
                json!({
                    "ok": {
                        "state": "response",
                        "stream_handle": "900",
                        "status": status,
                        "url": final_url,
                        "headers": headers,
                        "written": null
                    }
                }),
            )),
            host,
            1_700_000_000_000,
        )
        .unwrap();
    if let Some((read_op, Task::HttpRead { .. })) = host.tasks.front().cloned() {
        host.pop();
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
    } else {
        cmds
    }
}

/// Builds a synthetic 1-bit indexed palette PNG mirroring Baidu Passport's
/// `passport.baidu.com/v2/api/qrcode` output (`modules * scale` pixels, 7x7 finders).
fn build_synthetic_qr_png(modules: usize, scale: usize) -> (Vec<u8>, Vec<String>) {
    let mut grid = vec![vec![false; modules]; modules];
    for (ox, oy) in [(0, 0), (modules - 7, 0), (0, modules - 7)] {
        for dy in 0..7 {
            for dx in 0..7 {
                let border = dx == 0 || dx == 6 || dy == 0 || dy == 6;
                let center = (2..=4).contains(&dx) && (2..=4).contains(&dy);
                grid[oy + dy][ox + dx] = border || center;
            }
        }
    }
    // Checkerboard payload bits in center.
    for (y, row) in grid.iter_mut().enumerate().take(modules - 8).skip(8) {
        for (x, cell) in row.iter_mut().enumerate().take(modules - 8).skip(8) {
            *cell = (x + y) % 2 == 0;
        }
    }
    let expected: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|&d| if d { '1' } else { '0' }).collect())
        .collect();

    let w = modules * scale;
    let h = modules * scale;
    let stride = w.div_ceil(8);
    let mut raw = Vec::with_capacity(h * (stride + 1));
    for y in 0..h {
        raw.push(0); // filter = None
        let mut row_bytes = vec![0u8; stride];
        for x in 0..w {
            let dark = grid[y / scale][x / scale];
            // Palette 0 = black (dark), 1 = white (light), identical to Baidu PNG.
            if !dark {
                row_bytes[x / 8] |= 1 << (7 - (x % 8));
            }
        }
        raw.extend_from_slice(&row_bytes);
    }
    let idat = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);

    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let push_chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&0u32.to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[1, 3, 0, 0, 0]); // 1-bit palette
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"PLTE", &[0, 0, 0, 255, 255, 255]);
    push_chunk(&mut png, b"IDAT", &idat);
    push_chunk(&mut png, b"IEND", &[]);
    (png, expected)
}

#[test]
fn md5_sha1_devuid_and_locate_sign_match_baidupcs_rust_vectors() {
    assert_eq!(
        sign::hex_lower(&sign::md5_digest(b"")),
        "d41d8cd98f00b204e9800998ecf8427e"
    );
    assert_eq!(
        sign::hex_lower(&sign::md5_digest(b"abc")),
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(
        sign::hex_lower(&sign::sha1_digest(b"")),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    assert_eq!(
        sign::hex_lower(&sign::sha1_digest(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );

    // Exact test vectors from komorebiCarry/BaiduPCS-Rust/backend/src/sign/devuid.rs
    assert_eq!(
        sign::generate_devuid(""),
        "D41D8CD98F00B204E9800998ECF8427E|0"
    );
    assert_eq!(
        sign::generate_devuid("test"),
        "098F6BCD4621D373CADE4E832627B4F6|0"
    );

    let s1 = sign::LocateSign::new(123_456_789, "test_bduss", 1_699_999_999);
    let s2 = sign::LocateSign::new(123_456_789, "test_bduss", 1_699_999_999);
    assert_eq!(s1, s2);
    assert_eq!(s1.rand.len(), 40);
    assert!(s1.devuid.ends_with("|0"));
    assert!(s1.url_params().contains("time=1699999999"));
    assert!(s1.url_params().contains("devuid="));
    assert!(s1.url_params().contains("cuid="));
}

#[test]
fn qr_png_decoder_extracts_exact_49x49_baidu_matrix() {
    let (png, expected) = build_synthetic_qr_png(49, 5);
    let matrix = qr_png::decode_qr_png_to_matrix(&png).unwrap();
    assert_eq!(matrix.len(), 49);
    assert_eq!(matrix, expected);
}

#[test]
fn qr_login_browse_locate_multi_cdn_and_sibling_subtitles_work_end_to_end() {
    let mut plugin = BaiduPanPlugin::default();
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

    let open_cmds = plugin
        .event(
            win(HostEvent::WindowOpened { snapshot: None }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(
        open_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::SetSidebar { .. }))
    );

    // 1. Start QR login -> getqrcode JSONP -> qrcode 1-bit PNG -> auto_poll_qr timeout -> scanned -> confirmed -> qrbdusslogin
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
    let (get_qr_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        get_qr_op,
        200,
        "https://passport.baidu.com/v2/api/getqrcode",
        vec![("Set-Cookie".into(), "BAIDUID=BID123:FG=1; Path=/".into())],
        br#"tangram_guid_1({"imgurl":"passport.baidu.com/v2/api/qrcode?sign=SIGN999&lp=pc","errno":0,"sign":"SIGN999"})"#,
    );
    let (png_op, png_task) = host.pop();
    assert!(matches!(png_task, Task::HttpStart { url, .. } if url.contains("sign=SIGN999")));
    let (png_bytes, _) = build_synthetic_qr_png(49, 5);
    let qr_cmds = feed_http(
        &mut plugin,
        &mut host,
        png_op,
        200,
        "https://passport.baidu.com/v2/api/qrcode?sign=SIGN999",
        vec![],
        &png_bytes,
    );
    assert_eq!(plugin.windows[&Id(1)].qr.as_ref().unwrap().matrix.len(), 49);
    assert!(qr_cmds.iter().any(|c| match c {
        HostCommand::SetSidebar { panel: Some(p) } => p.sections.iter().any(|s| {
            s.widgets.iter().any(|w| {
                matches!(
                    w,
                    Widget::QrCode {
                        poll_action_id: Some(act),
                        ..
                    } if act == "auto_poll_qr"
                )
            })
        }),
        _ => false,
    }));

    // Comet long-poll timeout during auto-poll must NOT toast an error.
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
    let (poll_timeout_op, _) = host.pop();
    let timeout_cmds = plugin
        .event(
            win(done(
                poll_timeout_op,
                json!({"error": {"code": ErrorCode::Timeout as i32, "message": "Timeout"}}),
            )),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    assert!(timeout_cmds.is_empty());

    // Confirmed scan -> qrbdusslogin -> membership/user/info -> SecretWrite -> quota
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
    let (poll_ok_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        poll_ok_op,
        200,
        "https://passport.baidu.com/channel/unicast",
        vec![],
        br#"{"errno":0,"channel_v":"{\"status\":0,\"v\":\"TEMP_V_TOKEN\"}"}"#,
    );
    let (confirm_op, confirm_task) = host.pop();
    assert!(
        matches!(confirm_task, Task::HttpStart { follow_redirects: false, url, .. } if url.contains("qrbdusslogin"))
    );
    feed_http(
        &mut plugin,
        &mut host,
        confirm_op,
        200,
        "https://passport.baidu.com/v3/login/main/qrbdusslogin",
        vec![
            (
                "Set-Cookie".into(),
                "BDUSS=REAL_BDUSS_VAL; Path=/; HttpOnly".into(),
            ),
            ("Set-Cookie".into(), "STOKEN=REAL_STOKEN_VAL; Path=/".into()),
        ],
        b"",
    );
    let (uinfo_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        uinfo_op,
        200,
        "https://pan.baidu.com/rest/2.0/membership/user/info",
        vec![],
        br#"{"errno":0,"user_info":{"uk":88990011,"username":"IrisPanUser","is_svip":1}}"#,
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
    assert_eq!(plugin.session.uid, 88_990_011);
    assert_eq!(plugin.session.username.as_deref(), Some("IrisPanUser"));
    assert_eq!(plugin.session.vip_type, Some(2));
    assert!(
        login_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::SetSources { sources } if sources.iter().all(|s| !s.requires_auth)))
    );
    let (quota_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        quota_op,
        200,
        "https://pan.baidu.com/api/quota",
        vec![],
        br#"{"errno":0,"total":5497558138880,"used":1099511627776}"#,
    );
    assert_eq!(plugin.quota.as_ref().unwrap().total, 5_497_558_138_880);

    // 2. Browse folder with video + photo + sibling subtitle (.chs.srt)
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(20),
                source_id: "files".into(),
                folder_id: Some("/Movies".into()),
                page: None,
                query: None,
                filters: BTreeMap::new(),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (list_op, _) = host.pop();
    let list_cmds = feed_http(
        &mut plugin,
        &mut host,
        list_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        br#"{"errno":0,"list":[
            {"fs_id":10,"path":"/Movies/Sub","server_filename":"Sub","isdir":1,"size":0,"category":6,"server_mtime":1700000000},
            {"fs_id":11,"path":"/Movies/Demo.mkv","server_filename":"Demo.mkv","isdir":0,"size":1073741824,"category":1,"server_mtime":1700000100,"thumbs":{"url3":"http://thumbnail.baidupcs.com/t1.jpg"}},
            {"fs_id":12,"path":"/Movies/Demo.chs.srt","server_filename":"Demo.chs.srt","isdir":0,"size":4096,"category":6,"server_mtime":1700000100},
            {"fs_id":13,"path":"/Movies/Poster.heic","server_filename":"Poster.heic","isdir":0,"size":2097152,"category":3,"server_mtime":1700000200}
        ]}"#,
    );
    let [HostCommand::ProvideSourcePage { page, .. }] = list_cmds.as_slice() else {
        panic!("expected ProvideSourcePage");
    };
    assert_eq!(page.folder_id.as_deref(), Some("/Movies"));
    assert_eq!(page.parent_id.as_deref(), Some("/"));
    assert_eq!(page.entries.len(), 3); // folder + video + photo (subtitle cached silently)
    assert_eq!(page.entries[0].kind, SourceEntryKind::Folder);
    assert_eq!(page.entries[1].kind, SourceEntryKind::Video);
    assert_eq!(page.entries[2].kind, SourceEntryKind::Photo);
    assert_eq!(
        page.entries[1].thumbnail.as_ref().unwrap().url,
        "https://thumbnail.baidupcs.com/t1.jpg"
    );

    // 3. Resolve video via PCS Locate v4 -> prefers HTTPS non-nb.cache CDN + exposes HLS variants
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(30),
                source_id: "files".into(),
                folder_id: Some("/Movies".into()),
                entry_id: "bd:11:/Movies/Demo.mkv".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (loc_op, loc_task) = host.pop();
    assert!(
        matches!(loc_task, Task::HttpStart { method, url, .. } if method == "POST" && url.contains("method=locatedownload") && url.contains("rand="))
    );
    let res_cmds = feed_http(
        &mut plugin,
        &mut host,
        loc_op,
        200,
        "https://pcs.baidu.com/rest/2.0/pcs/file",
        vec![],
        br#"{"errno":0,"urls":[
            {"url":"https://nb.cache.baidupcs.com/file/slow","encrypt":0},
            {"url":"https://qdall01.baidupcs.com/file/fast_primary","encrypt":0},
            {"url":"https://bjbgp01.baidupcs.com/file/fast_backup","encrypt":0}
        ]}"#,
    );
    let [HostCommand::ResolveStream { media, .. }] = res_cmds.as_slice() else {
        panic!("expected ResolveStream");
    };
    let StreamSource::DirectHttp { primary, .. } = &media.stream.source else {
        panic!("expected DirectHttp");
    };
    assert_eq!(
        primary.url,
        "https://qdall01.baidupcs.com/file/fast_primary"
    );
    assert!(
        media
            .stream
            .variants
            .iter()
            .any(|v| v.id == "hls.M3U8_AUTO_1080")
    );

    // 4. MediaOpened automatically locates & attaches sibling Demo.chs.srt subtitle
    plugin
        .event(
            win(HostEvent::MediaOpened {
                generation: Generation::default(),
                snapshot: iris_plugin_sdk::api::v2::source::PlaybackSnapshot {
                    media: Some(Id(500)),
                    uri: Some(
                        "iris-plugin://community.baidupan/files/bd:11:/Movies/Demo.mkv".into(),
                    ),
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
        br#"{"errno":0,"urls":[{"url":"https://qdall01.baidupcs.com/file/demo_sub","encrypt":0}]}"#,
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
fn streaming_hls_handles_adtoken_challenge_and_compiles_clean_virtual_hls() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession {
            mode: auth::AuthMode::Cookie,
            bduss: "BDUSS_TEST".into(),
            uid: 12345,
            vip_type: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = MockHost::default();
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(41),
                source_id: "videos".into(),
                folder_id: None,
                entry_id: "bd:99:/Clip.mp4".into(),
                variant: Some("hls.M3U8_AUTO_1080".into()),
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (s1_op, _) = host.pop();
    // First call triggers errno=133 with adToken -> automatic retry.
    let retry_cmds = feed_http(
        &mut plugin,
        &mut host,
        s1_op,
        200,
        "https://pan.baidu.com/api/streaming",
        vec![],
        br#"{"errno":133,"adToken":"AD_TOKEN_XYZ"}"#,
    );
    assert!(retry_cmds.is_empty());
    let (s2_op, s2_task) = host.pop();
    assert!(matches!(s2_task, Task::HttpStart { url, .. } if url.contains("adToken=AD_TOKEN_XYZ")));

    let m3u8 = concat!(
        "#EXTM3U\n",
        "#EXT-X-VERSION:3\n",
        "#EXT-X-ALLOW-CACHE:NO\n",
        "#EXT-X-TARGETDURATION:10\n",
        "#EXT-X-MEDIA-SEQUENCE:0\n",
        "#EXTINF:9.500,\n",
        "https://cdn.baidupcs.com/ts/seg0.ts?sign=1\n",
        "#EXTINF:5.250,\n",
        "/ts/seg1.ts?sign=2\n",
        "#EXT-X-ENDLIST\n"
    );
    let hls_cmds = feed_http(
        &mut plugin,
        &mut host,
        s2_op,
        200,
        "https://cdn.baidupcs.com/m3u8/clip.m3u8",
        vec![],
        m3u8.as_bytes(),
    );
    let [HostCommand::ResolveStream { media, .. }] = hls_cmds.as_slice() else {
        panic!("expected ResolveStream VirtualHls");
    };
    let StreamSource::VirtualManifest {
        format: ManifestFormat::VirtualHls,
        document,
        context: None,
    } = &media.stream.source
    else {
        panic!("expected VirtualHls");
    };
    let doc_bytes = host.blobs.get(document).unwrap();
    let parsed: VirtualHls = serde_json::from_slice(doc_bytes).unwrap();
    assert!(!parsed.live);
    assert_eq!(parsed.tracks.len(), 1);
    assert_eq!(parsed.tracks[0].segments.len(), 2);
    assert_eq!(parsed.tracks[0].segments[0].duration_ms.0, 9500);
    assert_eq!(parsed.tracks[0].segments[1].duration_ms.0, 5250);
    assert_eq!(parsed.resources.len(), 2);
}

#[test]
fn openapi_mode_refreshes_expired_token_and_resolves_official_dlink_redirect() {
    let mut plugin = BaiduPanPlugin {
        session: StoredSession {
            mode: auth::AuthMode::OpenApi,
            access_token: "EXPIRED_AT".into(),
            refresh_token: "RT_OLD".into(),
            client_id: "APP_KEY".into(),
            client_secret: "APP_SEC".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut host = MockHost::default();
    plugin
        .event(
            win(HostEvent::SourceBrowse {
                request_id: Id(50),
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
    let (b1_op, _) = host.pop();
    // Expired token (errno=111) -> triggers oauth/2.0/token refresh
    let c1 = feed_http(
        &mut plugin,
        &mut host,
        b1_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        br#"{"errno":111,"errmsg":"Access token expired"}"#,
    );
    assert!(c1.is_empty());
    let (ref_op, ref_task) = host.pop();
    assert!(
        matches!(ref_task, Task::HttpStart { url, .. } if url.contains("openapi.baidu.com/oauth/2.0/token"))
    );
    feed_http(
        &mut plugin,
        &mut host,
        ref_op,
        200,
        "https://openapi.baidu.com/oauth/2.0/token",
        vec![],
        br#"{"access_token":"FRESH_AT","refresh_token":"RT_NEW"}"#,
    );
    assert_eq!(plugin.session.access_token, "FRESH_AT");
    let (_sec_op, sec_task) = host.pop();
    assert!(matches!(sec_task, Task::SecretWrite { .. }));
    let (b2_op, b2_task) = host.pop();
    assert!(
        matches!(b2_task, Task::HttpStart { url, .. } if url.contains("access_token=FRESH_AT"))
    );
    let page_cmds = feed_http(
        &mut plugin,
        &mut host,
        b2_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/file",
        vec![],
        br#"{"errno":0,"list":[{"fs_id":777,"path":"/Song.flac","server_filename":"Song.flac","isdir":0,"size":50000000,"category":2,"server_mtime":1700000000}]}"#,
    );
    assert!(matches!(
        page_cmds.as_slice(),
        [HostCommand::ProvideSourcePage { page, .. }] if page.entries.len() == 1 && page.entries[0].kind == SourceEntryKind::Audio
    ));

    // Official filemetas -> HEAD no-redirect -> Location with User-Agent: pan.baidu.com
    plugin
        .event(
            win(HostEvent::SourceResolve {
                request_id: Id(51),
                source_id: "files".into(),
                folder_id: Some("/".into()),
                entry_id: "bd:777:/Song.flac".into(),
                variant: None,
            }),
            &mut host,
            1_700_000_000_000,
        )
        .unwrap();
    let (meta_op, _) = host.pop();
    feed_http(
        &mut plugin,
        &mut host,
        meta_op,
        200,
        "https://pan.baidu.com/rest/2.0/xpan/multimedia",
        vec![],
        br#"{"errno":0,"list":[{"dlink":"https://d.pcs.baidu.com/file/abc?fid=777"}]}"#,
    );
    let (head_op, head_task) = host.pop();
    assert!(
        matches!(head_task, Task::HttpStart { method, follow_redirects: false, .. } if method == "HEAD")
    );
    let resolved = feed_http(
        &mut plugin,
        &mut host,
        head_op,
        302,
        "https://d.pcs.baidu.com/file/abc",
        vec![(
            "Location".into(),
            "https://qdall01.baidupcs.com/file/real_song.flac?sign=ok".into(),
        )],
        b"",
    );
    let [HostCommand::ResolveStream { media, .. }] = resolved.as_slice() else {
        panic!("expected ResolveStream");
    };
    let StreamSource::DirectHttp { primary, .. } = &media.stream.source else {
        panic!("expected DirectHttp");
    };
    assert_eq!(
        primary.url,
        "https://qdall01.baidupcs.com/file/real_song.flac?sign=ok"
    );
    assert!(
        primary
            .headers
            .iter()
            .any(|(k, v)| k == "User-Agent" && v == "pan.baidu.com")
    );
}

#[test]
fn cookie_parsing_sanitization_natural_sort_and_logout_behave_cleanly() {
    let sess =
        StoredSession::parse_cookie_input("BDUSS=abc123; STOKEN=st456; EXTRA=ignored").unwrap();
    assert_eq!(sess.bduss, "abc123");
    assert_eq!(sess.stoken, "st456");

    let bare = StoredSession::parse_cookie_input("BARE_BDUSS_VALUE").unwrap();
    assert_eq!(bare.bduss, "BARE_BDUSS_VALUE");

    assert_eq!(
        sign::sanitize_text("影片\u{202E}\n\t01  HD", 64),
        "影片 01 HD"
    );
    assert_eq!(
        media::natural_cmp("ep2.mp4", "ep10.mp4"),
        std::cmp::Ordering::Less
    );
    assert!(media::subtitle_matches_video("Movie.mkv", "Movie.chs.srt"));
    assert!(!media::subtitle_matches_video("Movie.mkv", "Other.srt"));

    let mut plugin = BaiduPanPlugin {
        session: sess,
        ..Default::default()
    };
    let mut host = MockHost::default();
    let cmds = plugin
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
    assert!(!plugin.session.is_authenticated());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, HostCommand::SetSources { sources } if sources.iter().all(|s| s.requires_auth)))
    );
}
