use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use iris_api::{
    ChromeLayoutSpec, HostCommand, HostEvent, MediaKind, OverlayNode, PROTOCOL_VERSION, Permission,
    PlaybackSnapshot, PluginManifest, SourceEntryKind, SourceManageAction, UiThemeSpec, Widget,
};
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::player::{RemoteMediaRequest, Session, format_time, prepare};
use iris_core::plugin::{
    DirtySlots, PluginManager, PluginSource, PluginStatus, PollReport, snapshot_from_session,
};
use iris_core::settings::Settings;
use iris_core::vault::CredentialVault;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

struct TestEnv {
    root: PathBuf,
    ext_dir: PathBuf,
    state_dir: PathBuf,
    vault: CredentialVault,
}

impl TestEnv {
    fn new(tag: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = std::process::id();
        let root = std::env::temp_dir().join(format!("iris-plugin-e2e-{tag}-{id}-{seq}"));
        let _ = std::fs::remove_dir_all(&root);
        let (ext_dir, state_dir) = (root.join("plugins"), root.join("plugin_state"));
        std::fs::create_dir_all(&ext_dir).unwrap();
        Self {
            root,
            ext_dir,
            state_dir,
            vault: CredentialVault::in_memory(),
        }
    }

    fn manager(&self, disabled: &[String]) -> PluginManager {
        PluginManager::with_dirs_and_vault(
            Some(&self.ext_dir),
            Some(self.state_dir.clone()),
            disabled,
            self.vault.clone(),
        )
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn python_available() -> bool {
    let bin = if cfg!(target_os = "windows") {
        "python"
    } else {
        "python3"
    };
    ProcessCommand::new(bin)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn manifest_with(id: &str, entry: Option<&str>, perms: Vec<Permission>) -> PluginManifest {
    PluginManifest {
        id: id.into(),
        name: id.into(),
        version: "0.1.0".into(),
        author: "Test".into(),
        description: String::new(),
        protocol: PROTOCOL_VERSION,
        entry: entry.map(Into::into),
        permissions: perms,
        ..Default::default()
    }
}

fn write_ext_plugin(ext_dir: &Path, folder: &str, manifest: &PluginManifest, py_code: &str) {
    let pdir = ext_dir.join(folder);
    std::fs::create_dir_all(&pdir).unwrap();
    let json = serde_json::to_string_pretty(manifest).unwrap();
    std::fs::write(pdir.join("plugin.json"), json).unwrap();
    if let Some(entry) = &manifest.entry {
        std::fs::write(pdir.join(entry), py_code).unwrap();
    }
}

fn cmd_ev(id: &str, snapshot: PlaybackSnapshot) -> HostEvent {
    HostEvent::Command {
        id: id.into(),
        snapshot,
    }
}

fn submit_ev(id: &str, pairs: &[(&str, &str)]) -> HostEvent {
    let values = pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    HostEvent::ModalSubmit {
        id: id.into(),
        values,
    }
}

fn manage_ev(source_id: &str, action: SourceManageAction) -> HostEvent {
    HostEvent::SourceManage {
        source_id: source_id.into(),
        action,
    }
}

fn poll_until(
    mgr: &mut PluginManager,
    mut done: impl FnMut(&PluginManager, &PollReport) -> bool,
) -> PollReport {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut acc = PollReport::default();
    while Instant::now() < deadline {
        acc.merge(mgr.poll());
        if done(mgr, &acc) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    acc
}

#[test]
fn builtins_listeners_snapshot_delivery_and_declarative_themes() {
    let env = TestEnv::new("builtins");
    let mut mgr = env.manager(&[]);
    assert!(mgr.has_state_listeners());
    assert!(
        mgr.source_providers()
            .iter()
            .any(|(p, s)| p == "iris.webdav" && s.add_command_id == "add_server")
    );

    mgr.set_enabled("iris.guides", false);
    mgr.set_enabled("iris.ab-loop", false);
    assert!(!mgr.has_state_listeners() && mgr.source_providers().len() == 1);

    let snap = PlaybackSnapshot {
        uri: Some("file:///demo/movie.mkv".into()),
        title: Some("movie.mkv".into()),
        kind: Some(MediaKind::Video),
        position: 10.0,
        duration: 120.0,
        ..Default::default()
    };
    mgr.set_enabled_with_snapshot("iris.guides", true, snap.clone());
    mgr.set_enabled_with_snapshot("iris.ab-loop", true, snap.clone());
    assert!(mgr.has_state_listeners());

    let rep = mgr.send_to("iris.guides", cmd_ev("thirds", snap.clone()));
    assert!(rep.dirty.overlay && rep.dirty.status && rep.dirty.sidebar);
    mgr.reload_with_snapshot(&[], snap.clone());
    assert_eq!(mgr.status_badges()[0].1.text, "三分线");

    assert_eq!(format_time(3665.0), "1:01:05");
    let (mut snap_a, mut snap_b) = (snap.clone(), snap);
    snap_a.position = 5.0;
    snap_b.position = 12.0;
    mgr.send_to("iris.ab-loop", cmd_ev("set_a", snap_a));
    mgr.send_to("iris.ab-loop", cmd_ev("set_b", snap_b));
    let tick = |pos: f64| HostEvent::Tick {
        position: pos,
        duration: 120.0,
        paused: false,
    };
    assert_eq!(mgr.broadcast(&tick(12.1)).commands.len(), 1);
    assert!(mgr.broadcast(&tick(12.2)).commands.is_empty());

    let mut m1 = manifest_with("acme.a-neon", None, vec![Permission::UiTheme]);
    m1.theme = Some(UiThemeSpec {
        window_bg: Some("#050608".into()),
        accent: Some("#00E5FF".into()),
        ..Default::default()
    });
    m1.chrome = Some(ChromeLayoutSpec {
        compact_controls: Some(true),
        hidden_dock_buttons: vec!["loop".into()],
        ..Default::default()
    });
    write_ext_plugin(&env.ext_dir, "neon", &m1, "");

    let mut m2 = manifest_with("acme.b-frame", None, vec![Permission::UiTheme]);
    m2.theme = Some(UiThemeSpec {
        corner_radius: Some(10.0),
        ..Default::default()
    });
    m2.chrome = Some(ChromeLayoutSpec {
        hide_titlebar: Some(true),
        floating_dock: Some(true),
        hidden_dock_buttons: vec!["loop".into(), "aspect".into()],
        ..Default::default()
    });
    write_ext_plugin(&env.ext_dir, "frame", &m2, "");

    mgr.reload(&[]);
    let (theme, chrome) = mgr.active_ui_overrides();
    assert_eq!(theme.unwrap().corner_radius, Some(10.0));
    assert_eq!(
        chrome.unwrap().hidden_dock_buttons,
        vec!["loop".to_string(), "aspect".to_string()]
    );
}

#[test]
fn snapshot_from_session_converts_local_file_uri_and_remote_metadata() {
    let env = TestEnv::new("snap");
    let local_path = env.root.join("iris_snapshot_demo.mp4");
    std::fs::write(&local_path, b"stub").unwrap();

    let mut session = Session::new().unwrap();
    let prepared = prepare(&local_path, None).unwrap();
    let resolved = prepared.target().unwrap().to_path_buf();
    session.open(prepared, 0.0).unwrap();

    let local_snap = snapshot_from_session(&session, true);
    assert_eq!(
        local_snap.uri,
        Some(MediaUri::Local(resolved).to_uri_string())
    );
    assert!(local_snap.favorite && local_snap.title.as_deref() == Some("iris_snapshot_demo.mp4"));

    let remote_req = RemoteMediaRequest {
        uri: MediaUri::Plugin {
            plugin_id: "acme.cloud".into(),
            source_id: "drive".into(),
            entry_id: "ep1".into(),
        },
        title: "Episode 01 (4K)".into(),
        playback_url: "https://cdn.example.com/ep1.mp4".into(),
        headers: BTreeMap::new(),
        kind: Kind::Video,
        is_live: true,
        photo: None,
        subtitles: Vec::new(),
    };
    session
        .open_remote(remote_req, &Settings::default(), 0.0)
        .unwrap();
    session.apply(&iris_core::player::SessionEvent::Buffering(true));
    let remote_snap = snapshot_from_session(&session, false);
    assert_eq!(
        remote_snap.uri.as_deref(),
        Some("iris-plugin://acme.cloud/drive/ep1")
    );
    assert!(remote_snap.is_live && remote_snap.buffering);
    session.shutdown();
}

#[test]
fn reserved_iris_namespace_rejected_and_builtin_secrets_physically_isolated() {
    let env = TestEnv::new("namespace-vault");
    env.vault
        .set_secret(
            "builtin.iris.webdav",
            "secret.home-nas",
            Some("super-secret-nas"),
        )
        .unwrap();

    let marker_file = env.root.join("evil_ran.txt");
    let evil_m = manifest_with("iris.evil", Some("evil.py"), vec![Permission::Secrets]);
    write_ext_plugin(
        &env.ext_dir,
        "evil",
        &evil_m,
        &format!("open({:?}, 'w').write('pwned')\n", marker_file),
    );
    let shadow_m = manifest_with("iris.webdav", Some("shadow.py"), vec![Permission::Secrets]);
    write_ext_plugin(&env.ext_dir, "shadow", &shadow_m, "import sys\n");

    let mut mgr = env.manager(&[]);
    let evil = mgr
        .list()
        .into_iter()
        .find(|p| p.manifest.id == "iris.evil")
        .unwrap();
    assert_eq!(
        evil.status,
        PluginStatus::Error("reserved 'iris.*' namespace".into())
    );
    mgr.set_enabled("iris.evil", false);
    mgr.set_enabled("iris.evil", true);
    assert!(!marker_file.exists());

    let webdav = mgr
        .list()
        .into_iter()
        .find(|p| p.manifest.id == "iris.webdav")
        .unwrap();
    assert_eq!(
        (webdav.source, webdav.status),
        (PluginSource::Builtin, PluginStatus::Running)
    );

    if !python_available() {
        return;
    }
    let snoop_py = r#"import json, sys
for raw in sys.stdin:
    ev = json.loads(raw).get("payload", {})
    if ev.get("event") == "init":
        leaked = len(ev.get("secrets") or {})
        cmds = [
            {"command": "set_secret", "key": "secret.home-nas", "value": "ext-overwrite"},
            {"command": "set_status", "badge": {"id": "s", "text": f"leaked={leaked}", "accent": False}},
        ]
        sys.stdout.write(json.dumps({"type": "commands", "commands": cmds}) + "\n")
        sys.stdout.flush()
    elif ev.get("event") == "shutdown":
        break
"#;
    let snoop_m = manifest_with(
        "acme.snoop",
        Some("main.py"),
        vec![Permission::Secrets, Permission::UiStatus],
    );
    write_ext_plugin(&env.ext_dir, "snoop", &snoop_m, snoop_py);
    mgr.reload(&[]);
    poll_until(&mut mgr, |m, _| {
        m.status_badges().iter().any(|(id, _)| *id == "acme.snoop")
    });
    let (_, badge) = mgr
        .status_badges()
        .into_iter()
        .find(|(id, _)| *id == "acme.snoop")
        .unwrap();
    assert_eq!(badge.text, "leaked=0");
    assert_eq!(
        env.vault
            .get_secret("builtin.iris.webdav", "secret.home-nas")
            .as_deref(),
        Some("super-secret-nas")
    );
    assert_eq!(
        env.vault
            .get_secret("ext.acme.snoop", "secret.home-nas")
            .as_deref(),
        Some("ext-overwrite")
    );
}

#[test]
fn dirty_slots_value_equality_and_fullscreen_zero_hz_guard() {
    let env = TestEnv::new("dirty-slots");
    let mut mgr = env.manager(&[]);

    let status_only = DirtySlots {
        status: true,
        ..Default::default()
    };
    assert!(!DirtySlots::should_notify_ui(
        status_only,
        true,
        false,
        false
    ));
    assert!(status_only.should_notify_ui(true, true, false));
    assert!(
        DirtySlots {
            overlay: true,
            ..Default::default()
        }
        .should_notify_ui(true, false, false)
    );

    if !python_available() {
        return;
    }
    let dup_py = r#"import json, sys
B = {"id": "b", "text": "STATIC", "accent": False}
for raw in sys.stdin:
    ev = json.loads(raw).get("payload", {})
    if ev.get("event") in ("init", "command"):
        sys.stdout.write(json.dumps({"type": "commands", "commands": [{"command": "set_status", "badge": B}]}) + "\n")
        sys.stdout.flush()
    elif ev.get("event") == "shutdown":
        break
"#;
    write_ext_plugin(
        &env.ext_dir,
        "dup",
        &manifest_with("acme.dup", Some("main.py"), vec![Permission::UiStatus]),
        dup_py,
    );
    mgr.reload(&[]);

    let first = poll_until(&mut mgr, |_, r| r.dirty.status).dirty;
    assert!(first.status && !DirtySlots::should_notify_ui(first, true, false, false));

    let _ = mgr.send_to("acme.dup", cmd_ev("repeat", PlaybackSnapshot::default()));
    thread::sleep(Duration::from_millis(120));
    let second = mgr.poll().dirty;
    assert!(second.is_empty() && !DirtySlots::should_notify_ui(second, true, false, false));
}

#[test]
fn external_stderr_250_lines_and_large_json_line_never_crash_or_drop() {
    if !python_available() {
        return;
    }
    let env = TestEnv::new("stderr-250");
    let chatty_py = r#"import json, sys
for raw in sys.stdin:
    ev = json.loads(raw).get("payload", {})
    if ev.get("event") == "init":
        for i in range(250):
            sys.stderr.write(f"diag {i} " + "x" * 120 + "\n")
        sys.stderr.flush()
    elif ev.get("event") == "command" and ev.get("id") == "ping":
        for i in range(150):
            sys.stderr.write(f"ping {i}\n")
        sys.stderr.flush()
        entries = [{"id": f"e{i}", "name": f"Item {i}.mp4", "kind": "video", "variants": []} for i in range(1200)]
        page = {"source_id": "s", "folder_id": None, "parent_id": None, "title": "Big", "entries": entries, "next_page": None, "authenticated": True, "auth_hint": None}
        cmds = [{"command": "set_status", "badge": {"id": "ok", "text": "ALIVE-400", "accent": True}}, {"command": "provide_source_page", "request_id": "big", "page": page}]
        sys.stdout.write(json.dumps({"type": "commands", "commands": cmds}) + "\n")
        sys.stdout.flush()
    elif ev.get("event") == "shutdown":
        break
"#;
    let chatty_m = manifest_with(
        "acme.chatty",
        Some("main.py"),
        vec![
            Permission::ReadState,
            Permission::UiStatus,
            Permission::MediaSource,
        ],
    );
    write_ext_plugin(&env.ext_dir, "chatty", &chatty_m, chatty_py);
    write_ext_plugin(
        &env.ext_dir,
        "stall",
        &manifest_with("acme.stall", Some("stall.py"), vec![Permission::ReadState]),
        "import time\ntime.sleep(10)\n",
    );

    let mut mgr = env.manager(&[]);
    let started = Instant::now();
    for _ in 0..500 {
        let _ = mgr.broadcast(&HostEvent::Tick {
            position: 1.0,
            duration: 60.0,
            paused: false,
        });
    }
    assert!(started.elapsed() < Duration::from_millis(250));

    let _ = mgr.send_to("acme.chatty", cmd_ev("ping", PlaybackSnapshot::default()));
    let rep = poll_until(&mut mgr, |m, r| {
        !m.status_badges().is_empty()
            && r.commands.iter().any(|(_, c)| matches!(c, HostCommand::ProvideSourcePage { page, .. } if page.entries.len() == 1200))
    });
    assert!(!rep.commands.is_empty() && mgr.status_badges()[0].1.text == "ALIVE-400");
}

#[test]
fn external_example_plugins_end_to_end_never_silently_skips_errors() {
    if !python_available() {
        return;
    }
    let env = TestEnv::new("examples-e2e");
    let examples_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
    let mut mgr = PluginManager::with_dirs_and_vault(
        Some(&examples_dir),
        Some(env.state_dir.clone()),
        &[],
        env.vault.clone(),
    );

    for expected in [
        "example.cinema-ui",
        "example.cloud-media-demo",
        "example.watch-stats",
    ] {
        let info = mgr
            .list()
            .into_iter()
            .find(|p| p.manifest.id == expected)
            .unwrap_or_else(|| panic!("missing {expected}"));
        if let PluginStatus::Error(err) = &info.status {
            panic!("example plugin {expected} failed: {err}");
        }
        assert_eq!(info.status, PluginStatus::Running);
    }

    poll_until(&mut mgr, |m, _| {
        !m.toolbar_buttons().is_empty() && m.sidebar_panels().len() >= 5
    });
    for p in mgr.list() {
        if let PluginStatus::Error(err) = &p.status {
            panic!("plugin {} crashed after init: {err}", p.manifest.id);
        }
    }

    let cinema_panel = mgr
        .sidebar_panels()
        .into_iter()
        .find(|(p, _)| *p == "example.cinema-ui")
        .unwrap()
        .1;
    assert!(
        cinema_panel
            .sections
            .iter()
            .flat_map(|s| &s.widgets)
            .any(|w| matches!(w, Widget::Slider { .. }))
    );

    let paused_snap = PlaybackSnapshot {
        paused: true,
        position: 5.0,
        duration: 100.0,
        ..Default::default()
    };
    let _ = mgr.send_to(
        "example.cinema-ui",
        HostEvent::Action {
            id: "apply_skin:frameless".into(),
            value: None,
            snapshot: paused_snap.clone(),
        },
    );
    let _ = mgr.send_to(
        "example.cinema-ui",
        HostEvent::Action {
            id: "toggle_hud".into(),
            value: Some("true".into()),
            snapshot: paused_snap,
        },
    );
    poll_until(&mut mgr, |m, _| {
        m.active_ui_overrides().1.is_some()
            && m.overlay_nodes()
                .iter()
                .any(|(p, _)| *p == "example.cinema-ui")
    });
    assert!(
        mgr.overlay_nodes()
            .iter()
            .any(|(p, n)| *p == "example.cinema-ui" && matches!(n, OverlayNode::Button { .. }))
    );

    let _ = mgr.poll();
    let _ = mgr.broadcast(&HostEvent::Tick {
        position: 5.4,
        duration: 100.0,
        paused: true,
    });
    thread::sleep(Duration::from_millis(120));
    assert!(!mgr.poll().dirty.overlay);

    let _ = mgr.send_to(
        "example.cloud-media-demo",
        cmd_ev("login", PlaybackSnapshot::default()),
    );
    poll_until(&mut mgr, |m, _| m.active_modal().is_some());
    assert_eq!(mgr.active_modal().unwrap().0, "example.cloud-media-demo");

    let _ = mgr.send_to(
        "example.cloud-media-demo",
        submit_ev("cloud-login", &[("api_token", "tok-test"), ("cdn", "cn")]),
    );
    poll_until(&mut mgr, |m, _| {
        m.active_modal().is_none()
            && env
                .vault
                .get_secret("ext.example.cloud-media-demo", "api_token")
                .is_some()
    });
    assert!(mgr.active_modal().is_none());
    assert_eq!(
        env.vault
            .get_secret("ext.example.cloud-media-demo", "api_token")
            .as_deref(),
        Some("tok-test")
    );

    mgr.reload(&[]);
    let res_ev = HostEvent::SourceResolve {
        request_id: "res-1".into(),
        source_id: "demo-drive".into(),
        folder_id: None,
        entry_id: "bbb".into(),
        variant: Some("1080p".into()),
    };
    let mut rep = mgr.send_to("example.cloud-media-demo", res_ev);
    rep.merge(poll_until(&mut mgr, |_, r| {
        r.commands
            .iter()
            .any(|(_, c)| matches!(c, HostCommand::ResolveStream { .. }))
    }));
    assert!(rep.commands.iter().any(|(pid, c)| {
        pid == "example.cloud-media-demo"
            && matches!(c, HostCommand::ResolveStream { request_id: Some(req), stream } if req == "res-1" && stream.headers.get("X-Cloud-CDN").map(String::as_str) == Some("cn"))
    }));
}

#[test]
fn builtin_webdav_plugin_modal_async_browse_mtime_and_subtitle_resolve_e2e() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let srv = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        let req = String::from_utf8_lossy(&buf[..n]);
        assert!(req.starts_with("PROPFIND /dav/Anime/ HTTP/1.1\r\n"));
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: basic ywxpy2u6cznjcjn0")
        );
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">
            <D:response><D:href>/dav/Anime/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/Season%202/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/ep01.mkv</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>10485760</D:getcontentlength><D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/ep01.zh-Hans.ass</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
        </D:multistatus>"#;
        let resp = format!(
            "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
            xml.len()
        );
        sock.write_all(resp.as_bytes()).unwrap();
    });

    let env = TestEnv::new("webdav-e2e");
    let (mut mgr, mut wake_rx) = PluginManager::load_with_dirs_and_vault(
        Some(&env.ext_dir),
        Some(env.state_dir.clone()),
        &[],
        env.vault.clone(),
    );

    assert!(
        mgr.send_to(
            "iris.webdav",
            cmd_ev("add_server", PlaybackSnapshot::default())
        )
        .dirty
        .modal
    );
    let modal_id = mgr.active_modal().unwrap().1.id.clone();

    let bad = mgr.send_to(
        "iris.webdav",
        submit_ev(
            &modal_id,
            &[("name", "Home NAS"), ("endpoint", "ftp://bad.example.com")],
        ),
    );
    assert!(
        mgr.active_modal().is_some()
            && bad
                .commands
                .iter()
                .any(|(_, c)| matches!(c, HostCommand::ShowToast { .. }))
    );

    let endpoint = format!("http://127.0.0.1:{port}/dav");
    let ok = mgr.send_to(
        "iris.webdav",
        submit_ev(
            &modal_id,
            &[
                ("name", "Home NAS"),
                ("endpoint", &endpoint),
                ("base_path", "/"),
                ("auth", "basic"),
                ("username", "alice"),
                ("secret", "s3cr3t"),
            ],
        ),
    );
    assert!(ok.dirty.sources && ok.dirty.modal && mgr.active_modal().is_none());
    let source_id = mgr.media_sources()[0].2.id.clone();
    assert!(mgr.media_sources()[0].2.configurable && mgr.media_sources()[0].2.removable);
    assert_eq!(
        env.vault
            .get_secret("builtin.iris.webdav", &format!("secret.{source_id}"))
            .as_deref(),
        Some("s3cr3t")
    );
    assert!(
        !std::fs::read_to_string(env.state_dir.join("iris.webdav.json"))
            .unwrap()
            .contains("s3cr3t")
    );

    let browse_ev = HostEvent::SourceBrowse {
        request_id: "b-1".into(),
        source_id: source_id.clone(),
        folder_id: Some("/Anime".into()),
        page: None,
        query: None,
    };
    assert!(mgr.send_to("iris.webdav", browse_ev).commands.is_empty());

    let browse_rep = poll_until(&mut mgr, |_, r| {
        r.commands
            .iter()
            .any(|(_, c)| matches!(c, HostCommand::ProvideSourcePage { .. }))
    });
    srv.join().unwrap();
    assert!(wake_rx.try_recv().is_ok());
    let (_, HostCommand::ProvideSourcePage { page, .. }) = &browse_rep.commands[0] else {
        panic!("expected ProvideSourcePage")
    };
    assert_eq!(
        (page.entries.len(), page.entries[0].kind),
        (2, SourceEntryKind::Folder)
    );
    assert_eq!(
        (page.entries[1].name.as_str(), page.entries[1].modified_secs),
        ("ep01.mkv", Some(1_445_412_480))
    );

    let res_ev = HostEvent::SourceResolve {
        request_id: "r-1".into(),
        source_id: source_id.clone(),
        folder_id: Some("/Anime".into()),
        entry_id: "/Anime/ep01.mkv".into(),
        variant: None,
    };
    let res = mgr.send_to("iris.webdav", res_ev);
    let (_, HostCommand::ResolveStream { request_id, stream }) = &res.commands[0] else {
        panic!("expected ResolveStream")
    };
    assert_eq!(request_id.as_deref(), Some("r-1"));
    assert_eq!(
        stream.url,
        format!("http://127.0.0.1:{port}/dav/Anime/ep01.mkv")
    );
    assert_eq!(
        stream.headers.get("Authorization").map(String::as_str),
        Some("Basic YWxpY2U6czNjcjN0")
    );
    assert_eq!(
        stream.subtitles[0].url,
        format!("http://127.0.0.1:{port}/dav/Anime/ep01.zh-Hans.ass")
    );

    mgr.reload(&[]);
    assert_eq!(mgr.media_sources().len(), 1);
    assert!(
        mgr.send_to(
            "iris.webdav",
            manage_ev(&source_id, SourceManageAction::Configure)
        )
        .dirty
        .modal
    );
    mgr.dismiss_modal("iris.webdav");
    assert!(
        mgr.send_to(
            "iris.webdav",
            manage_ev(&source_id, SourceManageAction::Remove)
        )
        .dirty
        .sources
    );
    assert!(mgr.media_sources().is_empty());
    assert_eq!(
        env.vault
            .get_secret("builtin.iris.webdav", &format!("secret.{source_id}")),
        None
    );
}
