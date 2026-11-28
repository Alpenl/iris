use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use iris_api::v2::Capability;
use iris_api::{CommandSpec, UiThemeSpec};
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    Chapter, Command as PlayerCommand, ExternalTrack, MonitorLutFilter, PlaybackSnapshotInput,
    PlaybackState, ScopedPlayback,
};
use iris_core::settings::Settings;

use crate::plugins::{PlayerProgress, PluginPlayer};

pub(super) struct TempDir(pub PathBuf);

impl TempDir {
    pub(super) fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "iris-app-catalog-test-{label}-{}-{id}",
            std::process::id()
        ));
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

pub(super) struct FakePlayer {
    pub state: PlaybackState,
    pub local_path: Option<PathBuf>,
    pub uri: Option<MediaUri>,
    pub title: Option<String>,
    pub share_identity: bool,
    pub scoped: Option<Arc<ScopedPlayback>>,
    pub progress: Option<PlayerProgress>,
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

    fn execute(&mut self, _command: PlayerCommand) -> anyhow::Result<()> {
        Ok(())
    }

    fn open_scoped(
        &mut self,
        scoped: Arc<ScopedPlayback>,
        uri: MediaUri,
        _settings: &Settings,
        _resume: f64,
    ) -> anyhow::Result<()> {
        self.title = Some(scoped.title.clone());
        self.scoped = Some(scoped);
        self.uri = Some(uri);
        Ok(())
    }

    fn set_private_identity(&mut self, private: bool) {
        self.share_identity = !private;
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(s) = self.scoped.take() {
            s.stream.stop();
        }
        Ok(())
    }

    fn release_scoped(&mut self) {
        if let Some(s) = self.scoped.take() {
            s.stream.stop();
        }
    }

    fn set_plugin_timeline(&mut self, _owner: String, _chapters: &[Chapter]) -> anyhow::Result<()> {
        Ok(())
    }

    fn attach_plugin_track(&mut self, _track: Arc<dyn ExternalTrack>) -> anyhow::Result<()> {
        Ok(())
    }

    fn revoke_plugin_tracks(&mut self, _owner: &str) {}

    fn set_monitor_lut(
        &mut self,
        _filter: Option<Arc<dyn MonitorLutFilter>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn flush_deferred(&mut self) {}
}

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

pub(super) struct LoopbackMediaServer {
    pub url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LoopbackMediaServer {
    pub(super) fn start(body: Vec<u8>) -> Self {
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
