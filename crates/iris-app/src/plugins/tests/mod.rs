mod concurrency;

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use futures::{FutureExt, channel::mpsc::UnboundedReceiver, executor::block_on};
use iris_api::v2::{
    Capability, Generation, HostCommand, Id, Reply,
    enhance::{Chapter as ApiChapter, HeatRange, SkipRange},
    source::{
        PlaylistEntry, ResolvedMedia, SourceDescriptor, SourceEntry, SourceFilter, SourcePage,
    },
    stream::{RemoteResource, StreamSource, StreamSpec},
};
use iris_api::{
    CommandSpec, PluginModal, SidebarPanel, SourceEntryKind, StreamVariant, UiThemeSpec, ViewTimer,
    Widget,
};
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    Chapter, Command as PlayerCommand, ExternalTrack, MonitorLutFilter, PlaybackSnapshotInput,
    PlaybackState, PlayerActionError, ScopedPlayback,
};
use iris_core::settings::Settings;
use iris_core::store::Store;
use iris_core::vault::CredentialVault;
use iris_plugin_host::{
    PluginHost, PluginStatus, REMOTE_THUMB_LEASE_BYTES, REMOTE_THUMB_SCRATCH_BYTES,
};

use super::{
    ActiveSource, MAX_REMOTE_THUMB_CONCURRENT, OpenUrlOutcome, PlayerProgress, PluginBatchOutcome,
    PluginContext, PluginController, PluginPlayer, PluginSideEffect, QUOTA_RETRY_COOLDOWN,
    REVIEW_EXPIRY, SOURCE_TIMEOUT, ThumbnailCache, can_seek_skip_range, run_view_timer,
};

pub(super) struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("iris-app-test-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        Self(iris_core::library::safe_resolve_path(&dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[track_caller]
pub(super) fn wait_until(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum RecordedOp {
    Execute(String),
    OpenScoped { uri: MediaUri, resume: f64 },
    SetPrivate(bool),
    Stop,
    ReleaseScoped,
    SetTimeline { owner: String, count: usize },
    AttachTrack(String),
    RevokeTracks(String),
    SetLut(bool),
    FlushDeferred,
}

pub(super) struct FakePlayer {
    pub state: PlaybackState,
    pub local_path: Option<PathBuf>,
    pub uri: Option<MediaUri>,
    pub title: Option<String>,
    pub share_identity: bool,
    pub scoped: Option<Arc<ScopedPlayback>>,
    pub progress: Option<PlayerProgress>,
    pub fail_execute: Option<PlayerActionError>,
    pub fail_open_scoped: bool,
    pub fail_timeline: bool,
    pub fail_attach: bool,
    pub fail_lut: bool,
    pub ops: Vec<RecordedOp>,
}

impl Default for FakePlayer {
    fn default() -> Self {
        Self {
            state: PlaybackState {
                loaded: true,
                duration: 120.0,
                time_pos: 12.5,
                ..Default::default()
            },
            local_path: None,
            uri: None,
            title: None,
            share_identity: true,
            scoped: None,
            progress: None,
            fail_execute: None,
            fail_open_scoped: false,
            fail_timeline: false,
            fail_attach: false,
            fail_lut: false,
            ops: Vec::new(),
        }
    }
}

impl PluginPlayer for FakePlayer {
    fn current_local_path(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        self.uri.clone()
    }

    fn current_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn may_share_current_identity(&self) -> bool {
        self.share_identity
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn has_scoped_stream(&self) -> bool {
        self.scoped.is_some()
    }

    fn playback_snapshot_input(&self) -> PlaybackSnapshotInput {
        PlaybackSnapshotInput {
            kind: Some(iris_core::library::Kind::Video),
            media_uri: self.uri.clone(),
            local_file_name: self
                .local_path
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned()),
            current_title: self.title.clone(),
            private_identity: !self.share_identity,
            position: self.state.time_pos,
            duration: self.state.duration,
            paused: self.state.paused,
            volume: self.state.volume,
            muted: self.state.muted,
            speed: self.state.speed,
            current_chapter: self.state.current_chapter,
            is_live: self.state.is_live,
            buffering: self.state.buffering,
        }
    }

    fn progress(&self) -> Option<PlayerProgress> {
        self.progress.clone()
    }

    fn execute(&mut self, command: PlayerCommand) -> anyhow::Result<()> {
        let label = match command {
            PlayerCommand::TogglePlayback => "TogglePlayback".into(),
            PlayerCommand::SetSpeed(s) => format!("SetSpeed({s})"),
            PlayerCommand::SeekAbsolute(s) => format!("SeekAbsolute({s})"),
            PlayerCommand::SeekRelative(s) => format!("SeekRelative({s})"),
            PlayerCommand::SetVolume(v) => format!("SetVolume({v})"),
            PlayerCommand::ToggleMute => "ToggleMute".into(),
            PlayerCommand::SetAbLoop { start, end } => format!("SetAbLoop({start},{end})"),
            PlayerCommand::ClearAbLoop => "ClearAbLoop".into(),
            _ => "Command".into(),
        };
        self.ops.push(RecordedOp::Execute(label));
        if let Some(err) = self.fail_execute {
            return Err(anyhow::Error::new(err));
        }
        Ok(())
    }

    fn open_scoped(
        &mut self,
        scoped: Arc<ScopedPlayback>,
        uri: MediaUri,
        _settings: &Settings,
        resume: f64,
    ) -> anyhow::Result<()> {
        if self.fail_open_scoped {
            return Err(anyhow::anyhow!("simulated open_scoped failure"));
        }
        self.ops.push(RecordedOp::OpenScoped {
            uri: uri.clone(),
            resume,
        });
        self.title = Some(scoped.title.clone());
        self.scoped = Some(scoped);
        self.uri = Some(uri);
        Ok(())
    }

    fn set_private_identity(&mut self, private: bool) {
        self.share_identity = !private;
        self.ops.push(RecordedOp::SetPrivate(private));
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(s) = self.scoped.take() {
            s.stream.stop();
        }
        self.ops.push(RecordedOp::Stop);
        Ok(())
    }

    fn release_scoped(&mut self) {
        if let Some(s) = self.scoped.take() {
            s.stream.stop();
        }
        self.ops.push(RecordedOp::ReleaseScoped);
    }

    fn set_plugin_timeline(&mut self, owner: String, chapters: &[Chapter]) -> anyhow::Result<()> {
        self.ops.push(RecordedOp::SetTimeline {
            owner,
            count: chapters.len(),
        });
        if self.fail_timeline {
            return Err(anyhow::anyhow!("timeline rejected"));
        }
        Ok(())
    }

    fn attach_plugin_track(&mut self, track: Arc<dyn ExternalTrack>) -> anyhow::Result<()> {
        self.ops
            .push(RecordedOp::AttachTrack(track.label().to_owned()));
        if self.fail_attach {
            return Err(anyhow::anyhow!("attach rejected"));
        }
        Ok(())
    }

    fn revoke_plugin_tracks(&mut self, owner: &str) {
        self.ops.push(RecordedOp::RevokeTracks(owner.to_owned()));
    }

    fn set_monitor_lut(&mut self, filter: Option<Arc<dyn MonitorLutFilter>>) -> anyhow::Result<()> {
        self.ops.push(RecordedOp::SetLut(filter.is_some()));
        if self.fail_lut {
            return Err(anyhow::anyhow!("lut rejected"));
        }
        Ok(())
    }

    fn flush_deferred(&mut self) {
        self.ops.push(RecordedOp::FlushDeferred);
    }
}

pub(super) struct AppRig {
    pub dir: TempDir,
    pub ctrl: PluginController,
    pub wake_rx: UnboundedReceiver<()>,
    pub player: FakePlayer,
    pub store: Store,
    pub settings: Settings,
    pub active_source: ActiveSource,
}

impl AppRig {
    pub fn new(label: &str, deferred: Vec<(String, String)>) -> Self {
        let dir = TempDir::new(label);
        let host =
            PluginHost::open_in(&dir.0, CredentialVault::in_memory(), &[], false).expect("host");
        let now = Instant::now();
        let (ctrl, wake_rx) = PluginController::open_window(host, &[], "graphite", deferred, now);
        let store = Store::load_from_dir(&dir.0);
        Self {
            dir,
            ctrl,
            wake_rx,
            player: FakePlayer::default(),
            store,
            settings: Settings::default(),
            active_source: ActiveSource::Local,
        }
    }

    pub fn poll(&mut self, now: Instant) -> PluginBatchOutcome {
        self.ctrl.poll(
            PluginContext {
                player: &mut self.player,
                store: &mut self.store,
                settings: &self.settings,
                active_source: &mut self.active_source,
            },
            now,
        )
    }

    pub fn install_zip(&mut self, name: &str, bytes: &[u8]) {
        let pkg_path = self.dir.0.join(format!("{name}.irisplugin"));
        std::fs::write(&pkg_path, bytes).unwrap();
        let out = self.ctrl.review_package_path(pkg_path);
        assert!(out.effects.iter().any(|e| matches!(
            e,
            PluginSideEffect::ShowToast { message, is_error: false } if message.contains("正在验证")
        )));
        wait_until("package review sheet", || {
            let _ = self.poll(Instant::now());
            self.ctrl.review_summary().is_some()
        });
        let approve = self.ctrl.approve_host_consent();
        assert!(approve.domain.consent_changed);
        wait_until("plugin ready", || {
            let live_ready = self.ctrl.is_ready(name);
            let _ = self.poll(Instant::now());
            live_ready
                && !self.ctrl.has_host_consent()
                && self
                    .ctrl
                    .list()
                    .iter()
                    .any(|p| p.id == name && p.status == PluginStatus::Ready)
        });
    }

    pub fn authorize_loopback_origin(&mut self, plugin_id: &str, cmd_id: &str) {
        let _ = self.ctrl.invoke_command(
            PluginContext {
                player: &mut self.player,
                store: &mut self.store,
                settings: &self.settings,
                active_source: &mut self.active_source,
            },
            plugin_id,
            cmd_id,
            Instant::now(),
        );
        wait_until("origin prompt", || {
            let _ = self.poll(Instant::now());
            self.ctrl.origin_summary().is_some()
        });
        let origin_id = self.ctrl.origin_summary().unwrap().id;
        assert!(self.ctrl.toggle_insecure_auth(origin_id));
        let _ = self.ctrl.approve_host_consent();
        wait_until("origin granted acked by guest", || {
            let _ = self.poll(Instant::now());
            !self.ctrl.has_host_consent()
                && self
                    .ctrl
                    .active_modal()
                    .is_some_and(|(p, m)| p == plugin_id && m.id == "origin_ack")
        });
        self.ctrl.window.dismiss_modal(plugin_id);
        self.ctrl.modal.on_cancel();
    }
}

impl Drop for AppRig {
    fn drop(&mut self) {
        self.ctrl.shutdown();
    }
}

/// Builds a Wasm module matching ordered substring rules `(needle_8_bytes, reply_template)` on `iris_on_event`.
///
/// Supports dynamic placeholders so parallel tests never depend on global `NEXT_HANDLE` values:
/// - `{{REQ}}`: echoes `"request_id":"<digits>"` from the incoming `HostEvent` JSON
/// - `{{SAVE_REQ}}`: records the incoming `"request_id":"<digits>"` and emits nothing
/// - `{{SAVED_REQ}}`: replays the previously recorded `"request_id"`
/// - `{{GRANT}}`: replays `"grant":"<digits>"` captured from `HostEvent::OriginGranted`
pub(super) fn build_routed_wat(rules: &[(&[u8; 8], &str)], fallback_json: &str) -> String {
    enum Seg {
        Lit(u32, u32),
        EchoReq,
        SaveReq,
        SavedReq,
        Grant,
    }

    let mut data_defs = String::new();
    let mut match_arms = String::new();
    let mut offset = 2048_u32;
    let encode_bytes = |s: &str| s.bytes().map(|b| format!("\\{b:02x}")).collect::<String>();

    let mut add_lit = |s: &str| -> (u32, u32) {
        if s.is_empty() {
            return (offset, 0);
        }
        let encoded = encode_bytes(s);
        let cur = offset;
        data_defs.push_str(&format!("(data (i32.const {cur}) \"{encoded}\")\n"));
        offset += (s.len() as u32 + 16) & !7;
        (cur, s.len() as u32)
    };

    let parse_template = |tpl: &str, add_lit: &mut dyn FnMut(&str) -> (u32, u32)| -> Vec<Seg> {
        let tokens = [
            ("{{REQ}}", 0u8),
            ("{{SAVE_REQ}}", 1u8),
            ("{{SAVED_REQ}}", 2u8),
            ("{{GRANT}}", 3u8),
        ];
        let mut segs = Vec::new();
        let mut rest = tpl;
        while !rest.is_empty() {
            let next = tokens
                .iter()
                .filter_map(|(tok, kind)| rest.find(tok).map(|pos| (pos, *tok, *kind)))
                .min_by_key(|(pos, _, _)| *pos);
            match next {
                Some((pos, tok, kind)) => {
                    if pos > 0 {
                        let (off, len) = add_lit(&rest[..pos]);
                        segs.push(Seg::Lit(off, len));
                    }
                    segs.push(match kind {
                        0 => Seg::EchoReq,
                        1 => Seg::SaveReq,
                        2 => Seg::SavedReq,
                        _ => Seg::Grant,
                    });
                    rest = &rest[pos + tok.len()..];
                }
                None => {
                    let (off, len) = add_lit(rest);
                    segs.push(Seg::Lit(off, len));
                    break;
                }
            }
        }
        segs
    };

    for (needle, tpl) in rules {
        let segs = parse_template(tpl, &mut add_lit);
        let pat = u64::from_le_bytes(**needle);
        let mut build_body = String::from("(local.set $cur (i32.const 16384))\n");
        for seg in segs {
            match seg {
                Seg::Lit(off, len) if len > 0 => {
                    build_body.push_str(&format!(
                        "local.get $cur i32.const {off} i32.const {len} memory.copy\n\
                         local.get $cur i32.const {len} i32.add local.set $cur\n"
                    ));
                }
                Seg::Lit(_, _) => {}
                Seg::EchoReq => {
                    build_body.push_str(
                        "local.get $req_pos i32.const 0 i32.ge_s if\n\
                         local.get $ptr local.get $req_pos i32.add local.get $cur call $copy_q\n\
                         local.get $cur i32.add local.set $cur end\n",
                    );
                }
                Seg::SaveReq => {
                    build_body.push_str(
                        "local.get $req_pos i32.const 0 i32.ge_s if\n\
                         local.get $ptr local.get $req_pos i32.add i32.const 60128 call $copy_q\n\
                         global.set $saved_req_len end\n",
                    );
                }
                Seg::SavedReq => {
                    build_body.push_str(
                        "local.get $cur i32.const 60128 global.get $saved_req_len memory.copy\n\
                         local.get $cur global.get $saved_req_len i32.add local.set $cur\n",
                    );
                }
                Seg::Grant => {
                    build_body.push_str(
                        "local.get $cur i32.const 60000 global.get $grant_len memory.copy\n\
                         local.get $cur global.get $grant_len i32.add local.set $cur\n",
                    );
                }
            }
        }
        match_arms.push_str(&format!(
            r#"
            local.get $ptr local.get $len i64.const {pat} call $has8 i32.const 0 i32.ge_s
            if
                {build_body}
                i64.const 16384 i64.const 32 i64.shl
                local.get $cur i32.const 16384 i32.sub i64.extend_i32_u i64.or return
            end
            "#
        ));
    }

    let (ack_off, ack_len) = add_lit(
        r#"{"ok":[{"command":"open_modal","modal":{"id":"origin_ack","title":"ack","widgets":[]}}]}"#,
    );
    let ack_ptr_len = (u64::from(ack_off) << 32) | u64::from(ack_len);
    let (fb_off, fb_len) = add_lit(fallback_json);
    let fb_ptr_len = (u64::from(fb_off) << 32) | u64::from(fb_len);
    let pat_grant = u64::from_le_bytes(*b"grant\":\"");
    let pat_req = u64::from_le_bytes(*b"st_id\":\"");

    format!(
        r#"(module
            (memory (export "memory") 4 8)
            (global $grant_len (mut i32) (i32.const 0))
            (global $saved_req_len (mut i32) (i32.const 0))
            {data_defs}
            (func (export "iris_abi_version") (result i32) i32.const 2)
            (func (export "iris_alloc") (param i32) (result i32) i32.const 49152)
            (func (export "iris_dealloc") (param i32 i32))
            (func $has8 (param $ptr i32) (param $len i32) (param $pat i64) (result i32)
                (local $i i32)
                (block $out (loop $lp
                    local.get $i i32.const 8 i32.add local.get $len i32.gt_u br_if $out
                    local.get $ptr local.get $i i32.add i64.load align=1 local.get $pat i64.eq
                    if local.get $i i32.const 8 i32.add return end
                    local.get $i i32.const 1 i32.add local.set $i br $lp))
                i32.const -1)
            (func $copy_q (param $src i32) (param $dst i32) (result i32)
                (local $n i32) (local $ch i32)
                (block $done (loop $lp
                    local.get $src local.get $n i32.add i32.load8_u local.set $ch
                    local.get $ch i32.const 34 i32.eq br_if $done
                    local.get $dst local.get $n i32.add local.get $ch i32.store8
                    local.get $n i32.const 1 i32.add local.set $n br $lp))
                local.get $n)
            (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
                (local $gpos i32) (local $req_pos i32) (local $cur i32)
                local.get $ptr local.get $len i64.const {pat_grant} call $has8 local.set $gpos
                local.get $gpos i32.const 0 i32.ge_s
                if
                    local.get $ptr local.get $gpos i32.add i32.const 60000 call $copy_q
                    global.set $grant_len
                    i64.const {ack_ptr_len}
                    return
                end
                local.get $ptr local.get $len i64.const {pat_req} call $has8 local.set $req_pos
                {match_arms}
                i64.const {fb_ptr_len})
        )"#
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn package_zip(
    id: &str,
    perms: &[Capability],
    commands: &[CommandSpec],
    url_patterns: &[&str],
    theme: Option<UiThemeSpec>,
    wat_src: &str,
) -> Vec<u8> {
    let wasm = wat::parse_str(wat_src).expect("valid WAT");
    let mut manifest = serde_json::json!({
        "id": id,
        "name": format!("Plugin {id}"),
        "version": "0.1.0",
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": perms,
        "commands": commands,
        "url_patterns": url_patterns,
    });
    if let Some(t) = theme {
        manifest["theme"] = serde_json::to_value(t).unwrap();
    }
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("plugin.json", opts).unwrap();
    zip.write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    zip.start_file("plugin.wasm", opts).unwrap();
    zip.write_all(&wasm).unwrap();
    zip.finish().unwrap().into_inner()
}

struct LoopbackMediaServer {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LoopbackMediaServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let worker = thread::spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                if let Ok((mut sock, _)) = listener.accept() {
                    let _ = sock.set_nonblocking(false);
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf);
                    let _ = write!(
                        sock,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(&body);
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for LoopbackMediaServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[test]
fn modal_submit_validation_failure_preserves_state_until_close_modal() {
    let open_modal_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![
            HostCommand::OpenModal {
                modal: Box::new(PluginModal {
                    id: "login".into(),
                    title: "登录账号".into(),
                    subtitle: None,
                    widgets: vec![Widget::Input {
                        id: "secret".into(),
                        label: Some("口令".into()),
                        value: "seed".into(),
                        placeholder: String::new(),
                        secret: true,
                        action_id: None,
                    }],
                    submit_label: Some("登录".into()),
                    cancel_label: None,
                    timer: Some(ViewTimer {
                        interval_ms: 1_000,
                        action_id: "poll_qr".into(),
                    }),
                }),
            },
            HostCommand::SetSidebar {
                panel: Some(Box::new(SidebarPanel {
                    title: "面板".into(),
                    sections: Vec::new(),
                    timer: Some(ViewTimer {
                        interval_ms: 2_500,
                        action_id: "sb_tick".into(),
                    }),
                })),
            },
        ],
    })
    .unwrap();
    let bad_pw_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ShowToast {
            message: "口令校验失败".into(),
        }],
    })
    .unwrap();
    let ok_pw_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::CloseModal {
            id: Some("login".into()),
        }],
    })
    .unwrap();
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();

    let wat = build_routed_wat(
        &[
            (b"open_mdl", &open_modal_reply),
            (b"bad-pass", &bad_pw_reply),
            (b"goodpass", &ok_pw_reply),
        ],
        &empty_reply,
    );
    let zip = package_zip(
        "demo.modal",
        &[
            Capability::UiModal,
            Capability::UiSidebar,
            Capability::Notifications,
        ],
        &[CommandSpec {
            id: "open_mdl".into(),
            title: "打开登录框".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        None,
        &wat,
    );

    // Deferred command `("demo.modal", "open_mdl")` fires automatically as soon as installation completes!
    let mut rig = AppRig::new(
        "modal-chain",
        vec![("demo.modal".into(), "open_mdl".into())],
    );
    rig.install_zip("demo.modal", &zip);

    wait_until("deferred command opens modal", || {
        let _ = rig.poll(Instant::now());
        rig.ctrl.active_modal().is_some()
    });
    assert!(rig.ctrl.modal_mut().take_focus_pending());
    assert_eq!(
        rig.ctrl.active_modal_timer(),
        Some(("demo.modal".into(), "poll_qr".into(), 1_000))
    );
    assert_eq!(
        rig.ctrl.sidebar_panel_timers(),
        vec![("demo.modal".into(), "sb_tick".into(), 2_500)]
    );

    // 1. Submit invalid value -> `is_submitting` blocks duplicate submit; `ShowToast` converts into inline modal error!
    let mut user_fields = BTreeMap::from([("secret".to_owned(), "bad-pass".to_owned())]);
    let submit_out = rig.ctrl.submit_active_modal(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        |_, _| user_fields.clone(),
        Instant::now(),
    );
    assert!(submit_out.domain.modal_changed);
    assert!(rig.ctrl.modal().is_submitting("demo.modal", "login"));
    // Duplicate submit while in flight is a no-op.
    let dup = rig.ctrl.submit_active_modal(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        |_, _| user_fields.clone(),
        Instant::now(),
    );
    assert!(!dup.domain.modal_changed);

    let mut toast_leaked = false;
    wait_until("validation error routed to modal", || {
        let batch = rig.poll(Instant::now());
        toast_leaked |= batch
            .effects
            .iter()
            .any(|e| matches!(e, PluginSideEffect::ShowToast { .. }));
        rig.ctrl.modal().error() == Some("口令校验失败")
    });
    assert!(
        !toast_leaked,
        "modal validation failure must not emit global toast"
    );
    assert!(!rig.ctrl.modal().is_submitting("demo.modal", "login"));
    assert!(
        rig.ctrl.active_modal().is_some(),
        "modal must stay open on error"
    );
    assert_eq!(
        user_fields.get("secret").map(String::as_str),
        Some("bad-pass")
    );

    // 2. User edits field -> inline error clears -> valid submit closes modal!
    rig.ctrl.modal_mut().clear_feedback();
    assert_eq!(rig.ctrl.modal().error(), None);
    user_fields.insert("secret".into(), "goodpass".into());
    let _ = rig.ctrl.submit_active_modal(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        |_, _| user_fields.clone(),
        Instant::now(),
    );
    wait_until("modal closed on valid submit", || {
        let _ = rig.poll(Instant::now());
        rig.ctrl.active_modal().is_none()
    });
}

#[test]
fn stream_admission_rolls_back_on_open_failure_and_commits_playlist_on_success() {
    let server = LoopbackMediaServer::start(b"FAKEVIDEO".to_vec());
    let init_sources = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "nas".into(),
                name: "NAS".into(),
                subtitle: None,
                badge: Some("NASBadge".into()),
                icon: Some("server".into()),
                supports_search: true,
                requires_auth: false,
                auth_command_id: None,
                configurable: false,
                removable: true,
            }],
        }],
    })
    .unwrap();
    let req_origin = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::RequestOrigin {
            request_id: Id(1),
            origin: server.url.clone(),
        }],
    })
    .unwrap();
    let page_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ProvideSourcePage {
            request_id: Id(1),
            page: Box::new(SourcePage {
                source_id: "nas".into(),
                folder_id: None,
                parent_id: None,
                title: "Root".into(),
                entries: vec![
                    SourceEntry {
                        id: "ep1".into(),
                        name: "Episode One".into(),
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: None,
                        duration: Some(90.0),
                        subtitle: None,
                        variants: vec![
                            StreamVariant {
                                id: "1080p".into(),
                                label: "1080P".into(),
                            },
                            StreamVariant {
                                id: "720p".into(),
                                label: "720P".into(),
                            },
                        ],
                        thumbnail: None,
                    },
                    SourceEntry {
                        id: "ep2".into(),
                        name: "Episode Two".into(),
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: None,
                        duration: Some(95.0),
                        subtitle: None,
                        variants: Vec::new(),
                        thumbnail: None,
                    },
                ],
                next_page: None,
                authenticated: true,
                auth_hint: None,
                filters: vec![SourceFilter {
                    id: "sort".into(),
                    label: "排序".into(),
                    options: vec![iris_api::SelectOption {
                        value: "new".into(),
                        label: "最新".into(),
                    }],
                    selected: Some("new".into()),
                }],
            }),
        }],
    })
    .unwrap()
    .replace("\"request_id\":\"1\"", "\"request_id\":\"{{REQ}}\"");
    let make_resolve = |entry_id: &str| {
        serde_json::to_string(&Reply::Ok {
            ok: vec![HostCommand::ResolveStream {
                request_id: Id(999_999_991),
                media: Box::new(ResolvedMedia {
                    stream: StreamSpec {
                        source_id: Some("nas".into()),
                        entry_id: Some(entry_id.into()),
                        title: Some(String::new()),
                        kind: Some(SourceEntryKind::Video),
                        is_live: false,
                        source: StreamSource::DirectHttp {
                            primary: RemoteResource {
                                url: format!("{}/{entry_id}.mp4", server.url),
                                headers: Vec::new(),
                                origin_grant: Some(Id(999_999_992)),
                            },
                            fallback_urls: Vec::new(),
                            identity: None,
                        },
                        variants: vec![
                            StreamVariant {
                                id: "1080p".into(),
                                label: "1080P".into(),
                            },
                            StreamVariant {
                                id: "720p".into(),
                                label: "720P".into(),
                            },
                        ],
                        active_variant: Some("1080p".into()),
                        audio_tracks: Vec::new(),
                        subtitles: Vec::new(),
                    },
                    playlist: vec![
                        PlaylistEntry {
                            source_id: "nas".into(),
                            entry_id: "ep1".into(),
                            title: "Episode One".into(),
                        },
                        PlaylistEntry {
                            source_id: "nas".into(),
                            entry_id: "ep2".into(),
                            title: "Episode Two".into(),
                        },
                    ],
                }),
            }],
        })
        .unwrap()
        .replace("999999991", "{{REQ}}")
        .replace("999999992", "{{GRANT}}")
    };
    let resolve_ep1 = make_resolve("ep1");
    let resolve_ep2 = make_resolve("ep2");
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();

    let wat = build_routed_wat(
        &[
            (b"w_opened", &init_sources),
            (b"auth_org", &req_origin),
            (b"ce_brows", &page_reply),
            (b"id\":\"ep1", &resolve_ep1),
            (b"id\":\"ep2", &resolve_ep2),
        ],
        &empty_reply,
    );
    let zip = package_zip(
        "demo.nas",
        &[
            Capability::MediaSource,
            Capability::NetHttp,
            Capability::UiModal,
        ],
        &[CommandSpec {
            id: "auth_org".into(),
            title: "授权地址".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        None,
        &wat,
    );

    let mut rig = AppRig::new("stream-chain", Vec::new());
    rig.install_zip("demo.nas", &zip);
    wait_until("sources registered", || {
        let _ = rig.poll(Instant::now());
        !rig.ctrl.media_sources().is_empty()
    });
    assert_eq!(rig.ctrl.source_badge("demo.nas", "nas"), "NASBadge");

    // Authorize loopback origin and wait until guest processes `OriginGranted`.
    rig.authorize_loopback_origin("demo.nas", "auth_org");

    // Browse source (`request_id = Id(1)`).
    let (started, browse_out) = rig.ctrl.browse_source(
        &mut rig.active_source,
        "demo.nas",
        "nas",
        None,
        None,
        None,
        Instant::now(),
    );
    assert!(started && browse_out.domain.catalog_changed);
    wait_until("browse page accepted", || {
        let _ = rig.poll(Instant::now());
        rig.ctrl.source_entries("nas").count() == 2
    });

    // 1. Failure path: `open_scoped` fails -> `admit_playback` rolls back active stream & cancels session!
    let target_uri = MediaUri::Plugin {
        plugin_id: "demo.nas".into(),
        source_id: "nas".into(),
        entry_id: "ep1".into(),
    };
    rig.store.remember(&target_uri, 33.0, 90.0, false);
    rig.player.fail_open_scoped = true;
    let _ = rig.ctrl.resolve_entry(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.nas",
        "nas",
        "ep1",
        Some("Fallback Ep1".into()),
        Some("1080p".into()),
        Instant::now(),
    );
    let mut saw_fail_effect = false;
    wait_until("failed open_scoped settles", || {
        let batch = rig.poll(Instant::now());
        saw_fail_effect |= batch
            .effects
            .contains(&PluginSideEffect::MediaOpenFailed { show_toast: true });
        saw_fail_effect
    });
    assert!(!rig.ctrl.window().has_active_stream_for(None));
    assert!(rig.ctrl.active_stream_meta().is_none());
    assert!(rig.ctrl.playlist().is_empty());

    // 2. Success path: `open_scoped` succeeds -> resume restored, variants & playlist registered!
    rig.player.fail_open_scoped = false;
    let _ = rig.ctrl.resolve_entry(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.nas",
        "nas",
        "ep1",
        Some("Fallback Ep1".into()),
        Some("1080p".into()),
        Instant::now(),
    );
    let mut saw_ok_effect = false;
    wait_until("successful open_scoped settles", || {
        let batch = rig.poll(Instant::now());
        saw_ok_effect |= batch
            .effects
            .contains(&PluginSideEffect::MediaOpenSucceeded);
        saw_ok_effect
    });
    assert!(rig.ctrl.window().has_active_stream_for(Some("demo.nas")));
    assert_eq!(rig.player.title.as_deref(), Some("Fallback Ep1"));
    assert!(rig.player.ops.contains(&RecordedOp::OpenScoped {
        uri: target_uri.clone(),
        resume: 33.0,
    }));
    let meta = rig.ctrl.active_stream_meta().expect("active stream meta");
    assert_eq!(meta.variants.len(), 2);
    assert_eq!(meta.active_variant.as_deref(), Some("1080p"));
    assert!(rig.ctrl.can_step_playlist(Some(&target_uri), 1));

    // Step playlist forward (`ep1` -> `ep2`).
    let step_out = rig
        .ctrl
        .step_playlist(
            PluginContext {
                player: &mut rig.player,
                store: &mut rig.store,
                settings: &rig.settings,
                active_source: &mut rig.active_source,
            },
            Some(&target_uri),
            1,
            false,
            Instant::now(),
        )
        .expect("step playlist");
    assert!(
        step_out
            .effects
            .contains(&PluginSideEffect::MediaResolveStarted)
    );
    wait_until("ep2 opened", || {
        let _ = rig.poll(Instant::now());
        rig.player.uri
            == Some(MediaUri::Plugin {
                plugin_id: "demo.nas".into(),
                source_id: "nas".into(),
                entry_id: "ep2".into(),
            })
    });

    // Non-wrapping step past end of playlist emits end-of-playlist toast without resolving.
    let ep2_uri = rig.player.uri.clone();
    let end_out = rig
        .ctrl
        .step_playlist(
            PluginContext {
                player: &mut rig.player,
                store: &mut rig.store,
                settings: &rig.settings,
                active_source: &mut rig.active_source,
            },
            ep2_uri.as_ref(),
            1,
            false,
            Instant::now(),
        )
        .unwrap();
    assert!(end_out.effects.iter().any(|e| matches!(
        e,
        PluginSideEffect::ShowToast { message, .. } if message.contains("已播完插件播放列表")
    )));

    // Disabling plugin cleans up active playback, stops player, and resets active_source to Local!
    let disable_out = rig.ctrl.toggle_plugin_enabled(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.nas",
        false,
        Instant::now(),
    );
    assert!(
        disable_out
            .effects
            .contains(&PluginSideEffect::PlaybackStopped)
    );
    assert!(rig.active_source.is_local());
    assert!(!rig.ctrl.window().has_active_stream_for(None));
}

#[test]
fn url_resolver_validates_inputs_ambiguity_and_enforces_private_identity() {
    let server = LoopbackMediaServer::start(b"RESOLVERVIDEO".to_vec());
    let req_origin = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::RequestOrigin {
            request_id: Id(1),
            origin: server.url.clone(),
        }],
    })
    .unwrap();
    let resolve_url_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ResolveStream {
            request_id: Id(999_999_991),
            media: Box::new(ResolvedMedia {
                stream: StreamSpec {
                    source_id: None,
                    entry_id: None,
                    title: Some("Resolved Private".into()),
                    kind: Some(SourceEntryKind::Video),
                    is_live: false,
                    source: StreamSource::DirectHttp {
                        primary: RemoteResource {
                            url: format!("{}/clip.mp4", server.url),
                            headers: Vec::new(),
                            origin_grant: Some(Id(999_999_992)),
                        },
                        fallback_urls: Vec::new(),
                        identity: None,
                    },
                    variants: Vec::new(),
                    active_variant: None,
                    audio_tracks: Vec::new(),
                    subtitles: Vec::new(),
                },
                playlist: Vec::new(),
            }),
        }],
    })
    .unwrap()
    .replace("999999991", "{{REQ}}")
    .replace("999999992", "{{GRANT}}");
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();

    let wat = build_routed_wat(
        &[
            (b"auth_org", &req_origin),
            (b"olve_url", &resolve_url_reply),
        ],
        &empty_reply,
    );
    let zip_a = package_zip(
        "demo.resa",
        &[
            Capability::UrlResolver,
            Capability::NetHttp,
            Capability::UiModal,
        ],
        &[CommandSpec {
            id: "auth_org".into(),
            title: "Auth".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &["https://video.example/*"],
        None,
        &wat,
    );
    let zip_b = package_zip(
        "demo.resb",
        &[Capability::UrlResolver],
        &[],
        &["https://video.example/*"],
        None,
        &wat,
    );

    let mut rig = AppRig::new("url-resolver", Vec::new());
    rig.install_zip("demo.resa", &zip_a);
    rig.install_zip("demo.resb", &zip_b);

    // Ambiguous auto match (>1 plugins) fails cleanly.
    let (ambig, _) = rig.ctrl.open_user_url(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "https://video.example/watch/1",
        "auto",
        Instant::now(),
    );
    assert!(matches!(ambig, OpenUrlOutcome::ValidationError(msg) if msg.contains("多个插件")));

    // Embedded credentials in URL are rejected before reaching the plugin.
    let (creds, _) = rig.ctrl.open_user_url(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "https://user:secret@video.example/watch/1",
        "demo.resa",
        Instant::now(),
    );
    assert!(matches!(creds, OpenUrlOutcome::ValidationError(msg) if msg.contains("账号密码")));

    // Explicit `"direct"` bypasses resolvers.
    let (direct, _) = rig.ctrl.open_user_url(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "https://cdn.example/raw.mp4",
        "direct",
        Instant::now(),
    );
    assert_eq!(
        direct,
        OpenUrlOutcome::OpenDirect {
            url: "https://cdn.example/raw.mp4".into()
        }
    );

    // Authorize loopback origin on `demo.resa`, then resolve explicitly -> private identity!
    rig.authorize_loopback_origin("demo.resa", "auth_org");

    let (resolved, _) = rig.ctrl.open_user_url(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "https://video.example/watch/42",
        "demo.resa",
        Instant::now(),
    );
    assert_eq!(resolved, OpenUrlOutcome::ResolvedByPlugin);
    wait_until("private url stream opened", || {
        let _ = rig.poll(Instant::now());
        rig.player.ops.contains(&RecordedOp::SetPrivate(true))
    });
    assert!(!rig.player.share_identity);
}

#[test]
fn consent_expiry_and_timeline_enhancement_failure_and_revocation_chains() {
    let timeline_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetTimelineMeta {
            media: Id(0),
            generation: Generation::default(),
            chapters: vec![ApiChapter {
                start_ms: 5_000.into(),
                title: "OP".into(),
            }],
            heat: vec![HeatRange {
                start_ms: 0.into(),
                end_ms: 10_000.into(),
                intensity: 80,
            }],
            skip: vec![SkipRange {
                start_ms: 5_000.into(),
                end_ms: 15_000.into(),
                label: "片头".into(),
            }],
        }],
    })
    .unwrap();
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(&[(b"set_time", &timeline_reply)], &empty_reply);
    let zip = package_zip(
        "demo.enhance",
        &[Capability::MediaEnhance, Capability::UiTheme],
        &[CommandSpec {
            id: "set_time".into(),
            title: "章节".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        Some(UiThemeSpec {
            name: Some("Custom".into()),
            accent: Some("#FF5B1F".into()),
            ..Default::default()
        }),
        &wat,
    );

    let mut rig = AppRig::new("enhance-consent", Vec::new());

    // Non-`.irisplugin` extension rejected immediately.
    let bad_ext = rig.ctrl.review_package_path(rig.dir.0.join("bad.zip"));
    assert!(bad_ext.effects.iter().any(|e| matches!(
        e,
        PluginSideEffect::ShowToast { message, .. } if message.contains(".irisplugin")
    )));

    // Start package review and verify 300s `REVIEW_EXPIRY` auto-dismisses without user action!
    let pkg_path = rig.dir.0.join("demo.enhance.irisplugin");
    std::fs::write(&pkg_path, &zip).unwrap();
    let t0 = Instant::now();
    let _ = rig.ctrl.review_package_path(pkg_path);
    wait_until("review arrives", || {
        let _ = rig.poll(t0);
        rig.ctrl.review_summary().is_some()
    });
    let expired_out = rig.ctrl.reconcile_lifetimes(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        t0 + REVIEW_EXPIRY,
    );
    assert!(expired_out.domain.consent_changed);
    assert!(!rig.ctrl.has_host_consent());

    // Now install cleanly and test `SetTimelineMeta` failure -> no leak, then success -> revocation!
    rig.install_zip("demo.enhance", &zip);
    rig.player.fail_timeline = true;
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.enhance",
        "set_time",
        Instant::now(),
    );
    let mut saw_timeline_err = false;
    wait_until("failed timeline surfaces toast", || {
        let batch = rig.poll(Instant::now());
        saw_timeline_err |= batch.effects.iter().any(|e| {
            matches!(e, PluginSideEffect::ShowToast { message, .. } if message.contains("插件操作未完成"))
        });
        saw_timeline_err
    });
    assert!(rig.ctrl.timeline_ranges().is_none());

    rig.player.fail_timeline = false;
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.enhance",
        "set_time",
        Instant::now(),
    );
    wait_until("timeline ranges committed", || {
        let _ = rig.poll(Instant::now());
        rig.ctrl.timeline_ranges().is_some()
    });
    let cur_gen = rig.ctrl.generation();
    assert!(can_seek_skip_range(cur_gen, cur_gen, true));
    assert!(!can_seek_skip_range(
        cur_gen,
        Generation {
            media: Id(cur_gen.media.0 + 1),
            ..cur_gen
        },
        true
    ));

    // `reset_theme_plugins_and_modals` disables `UiTheme` plugin -> next reconcile revokes tracks & flushes timeline!
    let (disabled_theme_ids, _) = rig.ctrl.reset_theme_plugins_and_modals(
        &mut rig.player,
        &mut rig.store,
        &mut rig.settings,
        &mut rig.active_source,
        Instant::now(),
    );
    assert_eq!(disabled_theme_ids, vec!["demo.enhance".to_owned()]);
    let mut timeline_reconciled = false;
    wait_until("dead enhancement binding reconciled", || {
        let batch = rig.poll(Instant::now());
        timeline_reconciled |= batch.domain.timeline_changed;
        timeline_reconciled && rig.ctrl.timeline_ranges().is_none()
    });
    assert!(rig.player.ops.contains(&RecordedOp::FlushDeferred));
    assert!(
        rig.player
            .ops
            .iter()
            .any(|op| matches!(op, RecordedOp::RevokeTracks(_)))
    );

    // Official `iris.*` uninstall guard blocks deletion cleanly.
    let (blocked, guard_out) = rig.ctrl.uninstall_plugin(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "iris.guides",
        false,
        Instant::now(),
    );
    assert!(blocked);
    assert!(guard_out.effects.iter().any(|e| matches!(
        e,
        PluginSideEffect::ShowToast { message, .. } if message.contains("官方内置插件")
    )));
}

#[test]
fn thumbnail_cache_and_view_timer_primitives_enforce_bounds_and_drop_safety() {
    assert_eq!(REMOTE_THUMB_SCRATCH_BYTES, 4 << 20);
    assert_eq!(REMOTE_THUMB_LEASE_BYTES, 128 << 10);

    let now = Instant::now();
    let mut cache: ThumbnailCache<u32> = ThumbnailCache::default();
    let key = ("src".to_owned(), "item".to_owned());
    assert!(cache.can_start(&key, now));
    cache.record_cooldown(key.clone(), now);
    assert!(!cache.can_start(&key, now));
    assert!(cache.can_start(&key, now + QUOTA_RETRY_COOLDOWN));
    assert_eq!(MAX_REMOTE_THUMB_CONCURRENT, 3);
    assert_eq!(SOURCE_TIMEOUT, Duration::from_secs(30));

    let ticks = std::cell::Cell::new(0usize);
    block_on(run_view_timer(
        || std::future::ready(()),
        || {
            let next = ticks.get() + 1;
            ticks.set(next);
            next < 605
        },
    ));
    assert_eq!(ticks.get(), 605);

    let dropped_ticks = std::cell::Cell::new(0usize);
    let fut = run_view_timer(std::future::pending::<()>, || {
        dropped_ticks.set(dropped_ticks.get() + 1);
        true
    });
    assert!(fut.now_or_never().is_none());
    assert_eq!(dropped_ticks.get(), 0);
}
