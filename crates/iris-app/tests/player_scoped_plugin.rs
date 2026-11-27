//! Cross-layer end-to-end integration tests exercising `iris_core::player::Session`
//! together with `iris_app::PluginController` and the public `iris_plugin_host` facade.
//!
//! Uses strictly public facade entry points (`PluginHost`, `WindowSession`, `Notice`,
//! `ReadyEffect`) with zero imports of internal `iris_plugin_host` submodules.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use iris_api::v2::{
    Capability, Generation, HostCommand, HostEvent, Id, Reply,
    enhance::{EnhancementResource, SubtitleFormat},
    source::{ResolvedMedia, SourceDescriptor, SourceEntry, SourcePage},
    stream::{Attachment, RemoteResource, StreamSource, StreamSpec},
};
use iris_api::{CommandSpec, SourceEntryKind};
use iris_core::{
    media_uri::MediaUri,
    player::{
        Command, ScopedPlayback, Session, SessionEvent, SessionUpdate, StreamProfile, TrackKind,
        prepare,
    },
    settings::{Hwdec, Settings},
    vault::CredentialVault,
};
use iris_plugin_host::{
    ActionKind, EnhancementOutcome, Notice, PluginHost, PluginStatus, ReadyEffect, WindowSession,
};
use ring::digest::{self, Context, SHA256};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("iris-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_until(timeout: Duration, label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain(player: &mut Session, events: &mut UnboundedReceiver<SessionUpdate>) {
    while let Ok(update) = events.try_recv() {
        player.apply_update(&update);
    }
}

fn plugin_uri(plugin_id: &str, source_id: &str, entry_id: &str) -> MediaUri {
    MediaUri::Plugin {
        plugin_id: plugin_id.into(),
        source_id: source_id.into(),
        entry_id: entry_id.into(),
    }
}

fn read_http_head(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let (mut header, mut byte) = (Vec::new(), [0]);
    while !header.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte)?;
        header.push(byte[0]);
        if header.len() > 16384 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    }
    Ok(header)
}

fn y4m_video(w: usize, h: usize, frames: usize) -> Vec<u8> {
    let frame_bytes = w * h * 3 / 2;
    format!(
        "YUV4MPEG2 W{w} H{h} F10:1 Ip A1:1 C420jpeg\n{}",
        format!("FRAME\n{}", "x".repeat(frame_bytes)).repeat(frames)
    )
    .into_bytes()
}

fn silent_wav() -> Vec<u8> {
    let data_bytes = 48_000u32 * 2 * 2;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    for chunk in [
        b"RIFF".as_slice(),
        &(36 + data_bytes).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &48_000u32.to_le_bytes(),
        &96_000u32.to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &data_bytes.to_le_bytes(),
    ] {
        out.extend_from_slice(chunk);
    }
    out.resize(out.len() + data_bytes as usize, 0);
    out
}

fn build_routed_wat(rules: &[(&[u8; 8], &str)], fallback_json: &str) -> String {
    enum Seg {
        Lit(u32, u32),
        EchoReq,
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
        let tokens = [("{{REQ}}", 0u8), ("{{GRANT}}", 1u8)];
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
                    segs.push(if kind == 0 { Seg::EchoReq } else { Seg::Grant });
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

/// Builds a Wasm module that creates `.cube` LUT temporary artifacts (`lut_1`, `lut_2`, `lut_3`)
/// via `iris_v2.host_submit` / `iris_v2.host_blob_new` and emits `HostCommand::SetMonitorLut`.
fn build_cube_lut_wat() -> String {
    let encode = |s: &str| s.bytes().map(|b| format!("\\{b:02x}")).collect::<String>();
    let begin_json = r#"{"operation":"artifact_begin","format":"cube"}"#;
    let empty_ok = r#"{"ok":[]}"#;
    let cube1 = format!("LUT_3D_SIZE 2\n{}", "1 0 0\n".repeat(8));
    let cube2 = format!("LUT_3D_SIZE 2\n{}", "0 1 0\n".repeat(8));
    let cube3 = format!("LUT_3D_SIZE 2\n{}", "0 0 1\n".repeat(8));
    let write_prefix = r#"{"operation":"artifact_write","handle":""#;
    let write_mid = r#"","blob":""#;
    let write_suffix = r#"","finish":true}"#;
    let cmd_prefix = r#"{"ok":[{"command":"set_monitor_lut","media":"0","generation":{"media":"0","seek":"0","context":"0"},"artifact":""#;
    let cmd_suffix = r#""}]}"#;

    format!(
        r#"(module
            (import "iris_v2" "host_submit" (func $submit (param i32 i32) (result i64)))
            (import "iris_v2" "host_blob_new" (func $blob_new (param i32 i32) (result i64)))
            (memory (export "memory") 4 8)
            (global $which (mut i32) (i32.const 0))
            (data (i32.const 1024) "{begin}")
            (data (i32.const 1152) "{empty}")
            (data (i32.const 1280) "{c1}")
            (data (i32.const 1408) "{c2}")
            (data (i32.const 1536) "{c3}")
            (data (i32.const 1664) "{wp}")
            (data (i32.const 1792) "{wm}")
            (data (i32.const 1856) "{ws}")
            (data (i32.const 1920) "{cp}")
            (data (i32.const 2112) "{cs}")
            (func (export "iris_abi_version") (result i32) i32.const 2)
            (func (export "iris_alloc") (param i32) (result i32) i32.const 32768)
            (func (export "iris_dealloc") (param i32 i32))
            (func $has8 (param $ptr i32) (param $len i32) (param $pat i64) (result i32)
                (local $i i32)
                (block $out (loop $lp
                    local.get $i i32.const 8 i32.add local.get $len i32.gt_u br_if $out
                    local.get $ptr local.get $i i32.add i64.load align=1 local.get $pat i64.eq
                    if local.get $i i32.const 8 i32.add return end
                    local.get $i i32.const 1 i32.add local.set $i br $lp))
                i32.const -1)
            (func $copy_until_quote (param $src i32) (param $dst i32) (result i32)
                (local $n i32) (local $ch i32)
                (block $done (loop $lp
                    local.get $src local.get $n i32.add i32.load8_u local.set $ch
                    local.get $ch i32.const 34 i32.eq br_if $done
                    local.get $dst local.get $n i32.add local.get $ch i32.store8
                    local.get $n i32.const 1 i32.add local.set $n br $lp))
                local.get $n)
            (func $u64_dec (param $val i64) (param $dst i32) (result i32)
                (local $tmp i32) (local $n i32)
                (local.set $tmp (i32.const 60000))
                (block $done (loop $lp
                    local.get $tmp local.get $n i32.add
                    local.get $val i64.const 10 i64.rem_u i32.wrap_i64 i32.const 48 i32.add
                    i32.store8
                    local.get $n i32.const 1 i32.add local.set $n
                    local.get $val i64.const 10 i64.div_u local.set $val
                    local.get $val i64.eqz br_if $done
                    br $lp))
                (local.set $tmp (i32.const 0))
                (block $rev (loop $rlp
                    local.get $tmp local.get $n i32.ge_u br_if $rev
                    local.get $dst local.get $tmp i32.add
                    i32.const 60000 local.get $n i32.add i32.const 1 i32.sub local.get $tmp i32.sub i32.load8_u
                    i32.store8
                    local.get $tmp i32.const 1 i32.add local.set $tmp br $rlp))
                local.get $n)
            (func (export "iris_on_event") (param $ptr i32) (param $len i32) (result i64)
                (local $pos i32) (local $cur i32) (local $blob i64) (local $cptr i32)
                ;; Commands "mk_lut_1", "mk_lut_2", "mk_lut_3"
                local.get $ptr local.get $len i64.const {pat_lut1} call $has8 i32.const 0 i32.ge_s
                if i32.const 0 global.set $which i32.const 1024 i32.const {begin_len} call $submit drop i64.const {empty_ret} return end
                local.get $ptr local.get $len i64.const {pat_lut2} call $has8 i32.const 0 i32.ge_s
                if i32.const 1 global.set $which i32.const 1024 i32.const {begin_len} call $submit drop i64.const {empty_ret} return end
                local.get $ptr local.get $len i64.const {pat_lut3} call $has8 i32.const 0 i32.ge_s
                if i32.const 2 global.set $which i32.const 1024 i32.const {begin_len} call $submit drop i64.const {empty_ret} return end

                ;; Completed `artifact_begin` -> `"artifact_writer":"<W>"`
                local.get $ptr local.get $len i64.const {pat_writer} call $has8 local.set $pos
                local.get $pos i32.const 0 i32.ge_s
                if
                    i32.const 1280 global.get $which i32.const 128 i32.mul i32.add local.set $cptr
                    local.get $cptr i32.const {cube_len} call $blob_new local.set $blob
                    i32.const 8192 i32.const 1664 i32.const {wp_len} memory.copy
                    i32.const 8192 i32.const {wp_len} i32.add local.set $cur
                    local.get $ptr local.get $pos i32.add i32.const 1 i32.add local.get $cur call $copy_until_quote
                    local.get $cur i32.add local.set $cur
                    local.get $cur i32.const 1792 i32.const {wm_len} memory.copy
                    local.get $cur i32.const {wm_len} i32.add local.set $cur
                    local.get $blob local.get $cur call $u64_dec local.get $cur i32.add local.set $cur
                    local.get $cur i32.const 1856 i32.const {ws_len} memory.copy
                    local.get $cur i32.const {ws_len} i32.add i32.const 8192 i32.sub local.set $cur
                    i32.const 8192 local.get $cur call $submit drop
                    i64.const {empty_ret} return
                end

                ;; Completed `artifact_write` -> `"artifact":"<A>"`
                local.get $ptr local.get $len i64.const {pat_artifact} call $has8 local.set $pos
                local.get $pos i32.const 0 i32.ge_s
                if
                    i32.const 12288 i32.const 1920 i32.const {cp_len} memory.copy
                    i32.const 12288 i32.const {cp_len} i32.add local.set $cur
                    local.get $ptr local.get $pos i32.add local.get $cur call $copy_until_quote
                    local.get $cur i32.add local.set $cur
                    local.get $cur i32.const 2112 i32.const {cs_len} memory.copy
                    local.get $cur i32.const {cs_len} i32.add i32.const 12288 i32.sub local.set $cur
                    i64.const 12288 i64.const 32 i64.shl local.get $cur i64.extend_i32_u i64.or return
                end

                i64.const {empty_ret})
        )"#,
        begin = encode(begin_json),
        begin_len = begin_json.len(),
        empty = encode(empty_ok),
        empty_ret = (1152_u64 << 32) | empty_ok.len() as u64,
        c1 = encode(&cube1),
        c2 = encode(&cube2),
        c3 = encode(&cube3),
        cube_len = cube1.len(),
        wp = encode(write_prefix),
        wp_len = write_prefix.len(),
        wm = encode(write_mid),
        wm_len = write_mid.len(),
        ws = encode(write_suffix),
        ws_len = write_suffix.len(),
        cp = encode(cmd_prefix),
        cp_len = cmd_prefix.len(),
        cs = encode(cmd_suffix),
        cs_len = cmd_suffix.len(),
        pat_lut1 = u64::from_le_bytes(*b"mk_lut_1"),
        pat_lut2 = u64::from_le_bytes(*b"mk_lut_2"),
        pat_lut3 = u64::from_le_bytes(*b"mk_lut_3"),
        pat_writer = u64::from_le_bytes(*b"writer\":"),
        pat_artifact = u64::from_le_bytes(*b"ifact\":\""),
    )
}

fn package_zip(id: &str, perms: &[Capability], commands: &[CommandSpec], wat_src: &str) -> Vec<u8> {
    let wasm = wat::parse_str(wat_src).expect("valid WAT");
    let manifest = serde_json::json!({
        "id": id,
        "name": id,
        "version": "0.1.0",
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": perms,
        "commands": commands,
    });
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("plugin.json", opts).unwrap();
    zip.write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    zip.start_file("plugin.wasm", opts).unwrap();
    zip.write_all(&wasm).unwrap();
    zip.finish().unwrap().into_inner()
}

fn install_and_ready(root: &Path, id: &str, zip_bytes: &[u8]) -> (PluginHost, WindowSession) {
    let host = PluginHost::open_in(root, CredentialVault::in_memory(), &[], false).unwrap();
    let (mut window, _wake) = host.open_window(&[], "graphite");
    let pkg_path = root.join(format!("{id}.irisplugin"));
    std::fs::write(&pkg_path, zip_bytes).unwrap();
    window.review_package(pkg_path).unwrap();
    let mut review_id = None;
    let mut grants = BTreeSet::new();
    wait_until(Duration::from_secs(10), "package review", || {
        for notice in window.poll().notices {
            if let Notice::Review(summary) = notice {
                review_id = Some(summary.id);
                grants = summary.manifest.permissions.iter().copied().collect();
            }
        }
        review_id.is_some()
    });
    window
        .approve_review(review_id.unwrap(), grants, false)
        .unwrap();
    wait_until(Duration::from_secs(10), "plugin ready", || {
        let live_ready = window.is_ready(id);
        let _ = window.poll();
        live_ready
            && window
                .list()
                .iter()
                .any(|p| p.id == id && p.status == PluginStatus::Ready)
    });
    (host, window)
}

fn confirm_loopback_origin(window: &mut WindowSession, plugin_id: &str, cmd_id: &str) {
    let _ = window.send_to(
        plugin_id,
        HostEvent::Command {
            id: cmd_id.into(),
            snapshot: None,
        },
    );
    let mut origin_id = None;
    wait_until(Duration::from_secs(10), "origin consent prompt", || {
        for notice in window.poll().notices {
            if let Notice::Origin(summary) = notice {
                origin_id = Some(summary.id);
            }
        }
        origin_id.is_some()
    });
    window
        .confirm_origin(origin_id.unwrap(), true, true)
        .unwrap();
    let mut saw_closed = false;
    wait_until(Duration::from_secs(10), "origin granted acked", || {
        saw_closed |= window
            .poll()
            .notices
            .iter()
            .any(|n| matches!(n, Notice::OriginClosed(_)));
        saw_closed
            && window
                .active_modal()
                .is_some_and(|(p, m)| p == plugin_id && m.id == "origin_ack")
    });
    window.dismiss_modal(plugin_id);
}

fn prepare_direct_http_via_facade(
    root: &Path,
    server_url: &str,
    plugin_id: &str,
    entry_name: &str,
    title: Option<String>,
    audio_tracks: Vec<Attachment>,
    subtitles: Vec<Attachment>,
) -> (PluginHost, WindowSession, Arc<ScopedPlayback>) {
    let init_sources = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "fixture".into(),
                name: "Fixture".into(),
                subtitle: None,
                badge: None,
                icon: None,
                supports_search: false,
                requires_auth: false,
                auth_command_id: None,
                configurable: false,
                removable: false,
            }],
        }],
    })
    .unwrap();
    let req_origin = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::RequestOrigin {
            request_id: Id(1),
            origin: server_url.into(),
        }],
    })
    .unwrap();
    let resolve_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ResolveStream {
            request_id: Id(999_999_991),
            media: Box::new(ResolvedMedia {
                stream: StreamSpec {
                    source_id: Some("fixture".into()),
                    entry_id: Some(entry_name.into()),
                    title,
                    kind: Some(SourceEntryKind::Video),
                    is_live: false,
                    source: StreamSource::DirectHttp {
                        primary: RemoteResource {
                            url: format!("{server_url}/{entry_name}"),
                            headers: Vec::new(),
                            origin_grant: Some(Id(999_999_992)),
                        },
                        fallback_urls: Vec::new(),
                        identity: None,
                    },
                    variants: Vec::new(),
                    active_variant: None,
                    audio_tracks,
                    subtitles,
                },
                playlist: Vec::new(),
            }),
        }],
    })
    .unwrap()
    .replace("999999991", "{{REQ}}")
    .replace("999999992", "{{GRANT}}");
    let empty_ok = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(
        &[
            (b"w_opened", &init_sources),
            (b"auth_org", &req_origin),
            (b"ce_resol", &resolve_reply),
        ],
        &empty_ok,
    );
    let zip = package_zip(
        plugin_id,
        &[
            Capability::MediaSource,
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
        &wat,
    );
    let (host, mut window) = install_and_ready(root, plugin_id, &zip);
    confirm_loopback_origin(&mut window, plugin_id, "auth_org");
    let req = window
        .resolve(plugin_id, "fixture", None, entry_name, None)
        .unwrap();
    let mut admitted_scoped = None;
    wait_until(
        Duration::from_secs(10),
        "direct_http stream resolve",
        || {
            let notices = window.poll().notices;
            for notice in notices {
                match notice {
                    Notice::Playback {
                        generation,
                        ticket,
                        playback,
                        ..
                    } if ticket.id == req => {
                        window
                            .admit_playback(plugin_id, generation, playback, |scoped| {
                                admitted_scoped = Some(scoped);
                                Ok::<_, ()>(())
                            })
                            .unwrap()
                            .unwrap();
                    }
                    Notice::Error { code, message, .. } => {
                        panic!("stream resolve failed: {code:?} {message:?}")
                    }
                    _ => {}
                }
            }
            admitted_scoped.is_some()
        },
    );
    (host, window, admitted_scoped.unwrap())
}

struct FixtureServer {
    url: String,
    stopped: Arc<AtomicBool>,
    requests: Arc<Mutex<BTreeSet<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
    fn start(directory: &Path) -> Self {
        let mut files = BTreeMap::new();
        let mut total = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file()
                && path.extension().is_some_and(|e| {
                    matches!(
                        e.to_str(),
                        Some("m3u8" | "mpd" | "ts" | "m4s" | "mp4" | "y4m" | "wav" | "vtt")
                    )
                })
            {
                let bytes = std::fs::read(&path).unwrap();
                total += bytes.len();
                assert!(total <= 64 << 20 && files.len() < 128);
                let name = path.file_name().unwrap().to_str().unwrap();
                files.insert(format!("/{name}"), bytes);
            }
        }
        assert!(!files.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(BTreeSet::new()));
        let (done, recorded) = (stopped.clone(), requests.clone());
        let thread = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if let Ok((mut socket, _)) = listener.accept() {
                    if let Err(error) = respond_fixture(&mut socket, &files, &recorded) {
                        eprintln!("fixture transport: {error}");
                    }
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            url,
            stopped,
            requests,
            thread: Some(thread),
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn respond_fixture(
    socket: &mut TcpStream,
    files: &BTreeMap<String, Vec<u8>>,
    requests: &Mutex<BTreeSet<String>>,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let header =
        String::from_utf8(read_http_head(socket)?).map_err(|_| std::io::ErrorKind::InvalidData)?;
    let mut fields = header.lines().next().unwrap().split_whitespace();
    let (method, path) = (fields.next().unwrap(), fields.next().unwrap());
    let Some(bytes) = files.get(path) else {
        return socket.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    };
    requests.lock().unwrap().insert(path.into());
    let range = header.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("range"))
            .map(|(_, v)| v.trim())
    });
    let (start, end) = match range {
        Some(value) => {
            let (a, b) = value
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let end = if b.is_empty() {
                bytes.len() - 1
            } else {
                b.parse::<usize>().unwrap().min(bytes.len() - 1)
            };
            (a.parse::<usize>().unwrap(), end)
        }
        None => (0, bytes.len() - 1),
    };
    if start >= bytes.len() || start > end {
        return write!(
            socket,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
    }
    let (status, range_hdr) = if range.is_some() {
        (
            "206 Partial Content",
            format!("Content-Range: bytes {start}-{end}/{}\r\n", bytes.len()),
        )
    } else {
        ("200 OK", String::new())
    };
    write!(
        socket,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture-v1\"\r\n{range_hdr}Connection: close\r\n\r\n",
        end - start + 1
    )?;
    if method != "HEAD" {
        socket.write_all(&bytes[start..=end])?;
    }
    Ok(())
}

fn prepare_attachment_effect(
    window: &mut WindowSession,
    plugin_id: &str,
    cmd_id: &str,
) -> Arc<dyn iris_core::player::ExternalTrack> {
    let _ = window.send_to(
        plugin_id,
        HostEvent::Command {
            id: cmd_id.into(),
            snapshot: None,
        },
    );
    let mut track = None;
    wait_until(Duration::from_secs(10), cmd_id, || {
        let report = window.poll();
        for action in report.actions {
            if let ActionKind::Enhancement(req) = action.kind {
                let res = window.execute_enhancement(req, true, 0, || {
                    Err(iris_core::store::AnnotationError::Denied)
                });
                assert!(matches!(res, EnhancementOutcome::Queued));
            }
        }
        for eff in report.effects {
            if let ReadyEffect::Attachment(ready) = eff {
                track = Some(ready.into_external_track());
            }
        }
        track.is_some()
    });
    track.unwrap()
}

fn prepare_lut_effect(
    window: &mut WindowSession,
    plugin_id: &str,
    cmd_id: &str,
) -> (String, Arc<dyn iris_core::player::MonitorLutFilter>) {
    let _ = window.send_to(
        plugin_id,
        HostEvent::Command {
            id: cmd_id.into(),
            snapshot: None,
        },
    );
    let mut filter = None;
    wait_until(Duration::from_secs(10), cmd_id, || {
        let report = window.poll();
        for action in report.actions {
            if let ActionKind::Enhancement(req) = action.kind {
                let res = window.execute_enhancement(req, true, 0, || {
                    Err(iris_core::store::AnnotationError::Denied)
                });
                assert!(matches!(res, EnhancementOutcome::Queued));
            }
        }
        for eff in report.effects {
            if let ReadyEffect::MonitorLut(ready) = eff {
                filter = Some(ready.into_filter());
            }
        }
        filter.is_some()
    });
    filter.unwrap()
}

#[test]
fn native_receipts_hold_revoked_attachments_and_release_accounting() {
    let root = TempDir::new("enhance");
    let media_dir = TempDir::new("enhance-media");
    std::fs::write(
        media_dir.join("sub.vtt"),
        b"WEBVTT\n\n00:00.000 --> 00:03.000\nReceipt\n",
    )
    .unwrap();
    let server = FixtureServer::start(&media_dir);

    let req_origin = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::RequestOrigin {
            request_id: Id(1),
            origin: server.url.clone(),
        }],
    })
    .unwrap();
    let attach_1 = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::AttachSubtitle {
            media: Id(0),
            generation: Generation::default(),
            id: "subtitle".into(),
            label: "Receipt".into(),
            lang: None,
            format: SubtitleFormat::WebVtt,
            resource: EnhancementResource::Remote {
                request: RemoteResource {
                    url: format!("{}/sub.vtt", server.url),
                    headers: Vec::new(),
                    origin_grant: Some(Id(999_999_992)),
                },
            },
        }],
    })
    .unwrap()
    .replace("999999992", "{{GRANT}}");
    let attach_2 = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::AttachSubtitle {
            media: Id(0),
            generation: Generation::default(),
            id: "subtitle-en".into(),
            label: "English".into(),
            lang: Some("en".into()),
            format: SubtitleFormat::WebVtt,
            resource: EnhancementResource::Remote {
                request: RemoteResource {
                    url: format!("{}/sub.vtt", server.url),
                    headers: Vec::new(),
                    origin_grant: Some(Id(999_999_992)),
                },
            },
        }],
    })
    .unwrap()
    .replace("999999992", "{{GRANT}}");
    let empty_ok = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(
        &[
            (b"auth_org", &req_origin),
            (b"attach_1", &attach_1),
            (b"attach_2", &attach_2),
        ],
        &empty_ok,
    );
    let zip = package_zip(
        "demo.sub",
        &[
            Capability::MediaEnhance,
            Capability::NetHttp,
            Capability::UiModal,
        ],
        &[
            CommandSpec {
                id: "auth_org".into(),
                title: "Auth".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "attach_1".into(),
                title: "Sub 1".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "attach_2".into(),
                title: "Sub 2".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
        ],
        &wat,
    );
    let (host, mut window) = install_and_ready(&root, "demo.sub", &zip);
    confirm_loopback_origin(&mut window, "demo.sub", "auth_org");

    let path = root.join("video.y4m");
    std::fs::write(&path, y4m_video(2, 2, 60)).unwrap();
    let mut player = Session::new_headless(true).unwrap();
    let mut events = player.events().unwrap();
    player.open(prepare(&path, None).unwrap(), 0.).unwrap();
    wait_until(Duration::from_secs(8), "media load", || {
        drain(&mut player, &mut events);
        player.state().loaded
    });

    let track1 = prepare_attachment_effect(&mut window, "demo.sub", "attach_1");
    let owner = track1.owner().to_owned();
    player.attach_plugin_track(track1).unwrap();
    wait_until(Duration::from_secs(8), "first plugin subtitle", || {
        drain(&mut player, &mut events);
        player.flush_deferred();
        player
            .selected_track(TrackKind::Sub)
            .is_some_and(|sub| sub.title == "Receipt")
    });
    let first_id = player.selected_track(TrackKind::Sub).unwrap().id;

    // Secondary plugin subtitle must not steal selection from the first.
    let track2 = prepare_attachment_effect(&mut window, "demo.sub", "attach_2");
    player.attach_plugin_track(track2).unwrap();
    wait_until(Duration::from_secs(8), "second plugin subtitle", || {
        drain(&mut player, &mut events);
        player.flush_deferred();
        player.tracks_of(TrackKind::Sub).len() == 2
    });
    assert_eq!(
        player.selected_track(TrackKind::Sub).map(|s| s.id),
        Some(first_id)
    );

    player.revoke_plugin_tracks(&owner);
    wait_until(Duration::from_secs(8), "revoked tracks removed", || {
        drain(&mut player, &mut events);
        player.tracks_of(TrackKind::Sub).is_empty()
    });

    // Revoke an addition before the asynchronous native acknowledgement arrives.
    let track3 = prepare_attachment_effect(&mut window, "demo.sub", "attach_1");
    player.attach_plugin_track(track3).unwrap();
    player.revoke_plugin_tracks(&owner);
    assert!(!host.is_ledger_empty());
    player.shutdown();
    drop((player, events));
    window.shutdown();
    host.shutdown();
    drop(window);
    wait_until(Duration::from_secs(8), "ledger cleanup", || {
        host.is_ledger_empty()
    });
}

#[test]
fn monitor_lut_artifact_loads_through_session_and_releases_ledger() {
    let parent = TempDir::new("lut-[,;' space");
    let wat = build_cube_lut_wat();
    let zip = package_zip(
        "demo.lut",
        &[Capability::MediaEnhance],
        &[CommandSpec {
            id: "mk_lut_1".into(),
            title: "LUT 1".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &wat,
    );
    let (host, mut window) = install_and_ready(&parent, "demo.lut", &zip);
    let (owner, lut) = prepare_lut_effect(&mut window, "demo.lut", "mk_lut_1");
    window.commit_lut_owner(owner);

    let clip_path = parent.join("clip.y4m");
    std::fs::write(&clip_path, y4m_video(16, 16, 30)).unwrap();
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session
        .open(prepare(&clip_path, None).unwrap(), 0.)
        .unwrap();
    wait_until(Duration::from_secs(5), "lut video load", || {
        drain(&mut session, &mut events);
        session.state().loaded
    });

    session.set_monitor_lut(Some(lut)).unwrap();
    wait_until(Duration::from_secs(5), "native monitor LUT filter", || {
        let d = session.backend_diagnostics();
        d.vf.contains("iris-monitor-lut") && d.out_pixel_format.starts_with("rgb")
    });
    session.stop().unwrap();
    session.shutdown();
    drop((session, events));
    window.shutdown();
    host.shutdown();
    drop(window);
    wait_until(Duration::from_secs(5), "lut ledger cleanup", || {
        host.is_ledger_empty()
    });
}

#[test]
fn rapid_lut_replacements_and_stop_hold_artifacts_until_native_receipts() {
    let parent = TempDir::new("lut-rapid");
    let wat = build_cube_lut_wat();
    let zip = package_zip(
        "demo.lutrapid",
        &[Capability::MediaEnhance],
        &[
            CommandSpec {
                id: "mk_lut_1".into(),
                title: "LUT 1".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "mk_lut_2".into(),
                title: "LUT 2".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "mk_lut_3".into(),
                title: "LUT 3".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
        ],
        &wat,
    );
    let (host, mut window) = install_and_ready(&parent, "demo.lutrapid", &zip);
    let (_, lut1) = prepare_lut_effect(&mut window, "demo.lutrapid", "mk_lut_1");
    let (_, lut2) = prepare_lut_effect(&mut window, "demo.lutrapid", "mk_lut_2");
    let (_, lut3) = prepare_lut_effect(&mut window, "demo.lutrapid", "mk_lut_3");

    let clip_path = parent.join("clip.y4m");
    std::fs::write(&clip_path, y4m_video(16, 16, 30)).unwrap();
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session
        .open(prepare(&clip_path, None).unwrap(), 0.)
        .unwrap();
    wait_until(Duration::from_secs(5), "rapid lut video load", || {
        drain(&mut session, &mut events);
        session.state().loaded
    });

    session.set_monitor_lut(Some(lut1)).unwrap();
    session.set_monitor_lut(Some(lut2)).unwrap();
    session.set_monitor_lut(Some(lut3)).unwrap();
    session.stop().unwrap();

    // Shut down the plugin runtime first so only `session`'s in-flight native LUT
    // receipts keep temporary `.cube` bytes alive until `drain` retires them.
    window.shutdown();
    host.shutdown();
    drop(window);
    wait_until(
        Duration::from_secs(5),
        "retired LUT native receipt cleanup",
        || {
            drain(&mut session, &mut events);
            host.is_ledger_empty()
        },
    );
    session.shutdown();
}

#[test]
fn actual_mpv_opens_scoped_video_audio_and_subtitles_then_restores_reference_policy() {
    let root = TempDir::new("mpv-scoped");
    let media_dir = TempDir::new("mpv-scoped-media");
    std::fs::write(media_dir.join("video.y4m"), y4m_video(2, 2, 60)).unwrap();
    std::fs::write(media_dir.join("audio.wav"), silent_wav()).unwrap();
    std::fs::write(
        media_dir.join("sub.vtt"),
        b"WEBVTT\n\n00:00.000 --> 00:02.000\nScoped subtitle\n",
    )
    .unwrap();
    let server = FixtureServer::start(&media_dir);

    let (host, window, scoped) = prepare_direct_http_via_facade(
        &root,
        &server.url,
        "demo.mpvscoped",
        "video.y4m",
        Some("Fixture".into()),
        vec![Attachment {
            id: "audio".into(),
            label: "Audio".into(),
            lang: Some("en".into()),
            default: true,
            resource: RemoteResource {
                url: format!("{}/audio.wav", server.url),
                headers: Vec::new(),
                origin_grant: Some(Id(999_999_992)),
            },
        }],
        vec![Attachment {
            id: "subtitle".into(),
            label: "Subtitle".into(),
            lang: Some("en".into()),
            default: true,
            resource: RemoteResource {
                url: format!("{}/sub.vtt", server.url),
                headers: Vec::new(),
                origin_grant: Some(Id(999_999_992)),
            },
        }],
    );

    let mut player = Session::new_headless(true).unwrap();
    let mut events = player.events().unwrap();
    let uri = plugin_uri("demo.mpvscoped", "fixture", "video.y4m");
    player
        .open_scoped(scoped.clone(), uri, &Settings::default(), 0.)
        .unwrap();
    wait_until(
        Duration::from_secs(8),
        "scoped video/audio/sub load",
        || {
            drain(&mut player, &mut events);
            player.state().loaded && player.backend_diagnostics().track_count >= 3
        },
    );
    let diag = player.backend_diagnostics();
    assert_eq!(diag.access_references, "no");
    assert_eq!(diag.demuxer, "lavf");
    assert!(diag.paused);
    assert!(
        player
            .execute(Command::SetAbLoop {
                start: f64::NAN,
                end: 1.
            })
            .is_err()
    );
    player
        .execute(Command::SetAbLoop {
            start: 0.2,
            end: 0.6,
        })
        .unwrap();
    player.execute(Command::TogglePlayback).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut previous, mut loops) = (0., 0);
    while loops < 3 {
        drain(&mut player, &mut events);
        let position = player.state().time_pos;
        if previous > position + 0.15 {
            loops += 1;
        }
        previous = position;
        assert!(
            Instant::now() < deadline,
            "native A-B did not repeat: loops={loops}, position={position}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.state().ab_start, Some(0.2));
    assert_eq!(player.state().ab_end, Some(0.6));
    player.execute(Command::ClearAbLoop).unwrap();
    let mut cleared = false;
    wait_until(Duration::from_secs(2), "A-B loop clear", || {
        while let Ok(update) = events.try_recv() {
            if matches!(
                player.apply_update(&update),
                Some(SessionEvent::AbEnd(None))
            ) {
                cleared = true;
            }
        }
        cleared
    });
    player.execute(Command::SeekAbsolute(1.)).unwrap();
    player.stop().unwrap();
    player.release_scoped();
    player.shutdown();
    window.shutdown();
    host.shutdown();
    drop((player, events, scoped, window));
    wait_until(Duration::from_secs(5), "scoped host ledger empty", || {
        host.is_ledger_empty()
    });
}

#[test]
#[ignore = "run scripts/test-plugin-v2-playback.py with a real FFmpeg fixture generator"]
fn real_hls_decode_through_scoped_rewritten_manifest() {
    let directory = std::env::var_os("IRIS_V2_ADAPTIVE_FIXTURE").expect("native fixture directory");
    let server = FixtureServer::start(Path::new(&directory));
    let root = TempDir::new("adaptive-host");
    let name = "hls.m3u8";
    {
        let (host, window, scoped) = prepare_direct_http_via_facade(
            &root,
            &server.url,
            "demo.adaptive",
            name,
            Some(name.into()),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(scoped.profile, Some(StreamProfile::Hls));
        let mut player = Session::new_headless(false).unwrap();
        player.set_hwdec(Hwdec::Off);
        let init_diag = player.backend_diagnostics();
        println!(
            "{name}: mpv={} ffmpeg={}",
            init_diag.mpv_version, init_diag.ffmpeg_version
        );
        let mut events = player.events().unwrap();
        let uri = plugin_uri("demo.adaptive", "fixture", name);
        player
            .open_scoped(scoped.clone(), uri, &Settings::default(), 0.)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let (mut samples, mut sought) = (Vec::<f64>::new(), false);
        loop {
            drain(&mut player, &mut events);
            if player.state().loaded && player.state().time_pos > 0.5 {
                let sync = player.backend_diagnostics().avsync.expect("finite avsync");
                samples.push(sync.abs());
                assert_eq!(player.state().video_size.0, 160);
                assert_eq!(player.media_stats().audio_sample_rate, Some(48_000));
                if !sought && player.state().time_pos > 1.0 {
                    player.execute(Command::SeekAbsolute(3.)).unwrap();
                    sought = true;
                }
                if sought && player.state().time_pos > 4.5 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{name} stalled: loaded={} position={} format={:?}",
                player.state().loaded,
                player.state().time_pos,
                player.backend_diagnostics().file_format
            );
            thread::sleep(Duration::from_millis(20));
        }
        samples.sort_by(f64::total_cmp);
        let p95 = samples[samples.len() * 95 / 100];
        assert!(
            samples.len() >= 20 && p95 < 0.15,
            "{name} avsync p95 {p95}, n={}",
            samples.len()
        );
        println!(
            "{name}: native decode/seek passed; null output |avsync| p95={p95:.6}s n={}",
            samples.len()
        );
        assert_eq!(player.backend_diagnostics().access_references, "no");
        player.stop().unwrap();
        player.release_scoped();
        player.shutdown();
        window.shutdown();
        host.shutdown();
        drop((player, events, scoped, window));
        wait_until(Duration::from_secs(5), "adaptive quiescence", || {
            host.is_ledger_empty()
        });
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().any(|p| p.starts_with("/hls-")));
}

#[test]
#[ignore = "run scripts/test-plugin-v2-transport.py; takes ten minutes"]
fn sustained_4k_scoped_transport() {
    let directory = std::env::var_os("IRIS_V2_TRANSPORT_FIXTURE").expect("4K fixture directory");
    let directory = Path::new(&directory);
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("fixture.json")).unwrap()).unwrap();
    let reference = std::fs::read(directory.join("reference.mp4")).unwrap();
    assert!(reference.len() <= 32 << 20);
    let digest = digest::digest(&SHA256, &reference);
    let peak = metadata["peak_one_second_bytes"].as_u64().unwrap();
    assert!(peak > 0 && metadata["width"] == 3840 && metadata["height"] == 2160);
    let budget = Duration::from_secs_f64(reference.len() as f64 / (peak as f64 * 1.25));
    drop(reference);

    let root = TempDir::new("transport-host");
    let server = FixtureServer::start(directory);
    let (host, window, scoped) = prepare_direct_http_via_facade(
        &root,
        &server.url,
        "demo.transport",
        "reference.mp4",
        None,
        Vec::new(),
        Vec::new(),
    );
    let url = url::Url::parse(&scoped.url).unwrap();
    let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let started = Instant::now();
    let (mut samples, mut total, mut misses, mut peak_host) = (Vec::new(), 0_u64, 0, 0);
    let mut next_progress = Duration::from_secs(60);
    while started.elapsed() < Duration::from_secs(600) {
        let start = Instant::now();
        let mut socket = TcpStream::connect(&authority).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            socket,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
            url.path()
        )
        .unwrap();
        assert!(
            read_http_head(&mut socket)
                .unwrap()
                .starts_with(b"HTTP/1.1 200")
        );
        let (mut hash, mut bytes, mut buffer) = (Context::new(&SHA256), 0_u64, [0_u8; 65536]);
        loop {
            let length = socket.read(&mut buffer).unwrap();
            if length == 0 {
                break;
            }
            hash.update(&buffer[..length]);
            bytes += length as u64;
            assert!(bytes <= 32 << 20);
            peak_host = peak_host.max(host.host_bytes_used());
        }
        assert_eq!(hash.finish().as_ref(), digest.as_ref());
        let elapsed = start.elapsed();
        misses += usize::from(elapsed > budget);
        total += bytes;
        samples.push(serde_json::json!({
            "bytes": bytes,
            "fetch_ms": elapsed.as_secs_f64() * 1000.,
            "bytes_per_second": bytes as f64 / elapsed.as_secs_f64(),
            "deadline_missed": elapsed > budget
        }));
        if started.elapsed() >= next_progress {
            println!(
                "transport: {:.0}s, {} reads, {misses} deadline misses",
                started.elapsed().as_secs_f64(),
                samples.len()
            );
            next_progress += Duration::from_secs(60);
        }
        thread::sleep(
            budget
                .saturating_sub(start.elapsed())
                .min(Duration::from_secs(600).saturating_sub(started.elapsed())),
        );
    }
    let elapsed = started.elapsed().as_secs_f64();
    let cleanup = Instant::now();
    scoped.stream.stop();
    window.shutdown();
    host.shutdown();
    drop((scoped, window));
    wait_until(Duration::from_secs(5), "transport quiescence", || {
        host.is_ledger_empty()
    });
    let receipt = serde_json::json!({
        "elapsed_seconds": elapsed,
        "total_bytes": total,
        "target_bytes_per_second": peak as f64 * 1.25,
        "paced_bytes_per_second": total as f64 / elapsed,
        "deadline_misses": misses,
        "samples": samples,
        "sampled_host_ledger_bytes": peak_host,
        "cleanup_ms": cleanup.elapsed().as_secs_f64() * 1000.,
        "debug": cfg!(debug_assertions),
        "scope": "DirectHttp local RAM origin + authorized relay; repeated complete 4K sample; no decoder, display, Wasm transformation or WAN; deadline misses are transport-budget misses, not player underruns"
    });
    println!("IRIS_V2_TRANSPORT_RECEIPT={receipt}");
    assert_eq!(misses, 0, "transport failed 1.25x peak-one-second budget");
}

#[allow(dead_code)]
fn _keep_source_page_types(_: SourcePage, _: SourceEntry) {}

#[path = "player_scoped_plugin/live.rs"]
mod live;
