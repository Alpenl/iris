//! Concurrency, cancellation, late-arriving reply, generation filtering, and
//! deterministic side-effect execution-order tests (Checkpoint Criterion C4).

use std::time::{Duration, Instant};

use iris_api::CommandSpec;
use iris_api::v2::{
    Capability, Generation, HostCommand, Id, Reply,
    enhance::Chapter as ApiChapter,
    source::{SourceDescriptor, SourceEntry, SourcePage},
};

use super::{
    ActiveSource, AppRig, PluginContext, PluginSideEffect, RecordedOp, SOURCE_TIMEOUT,
    build_routed_wat, package_zip, wait_until,
};

#[test]
fn cancelled_and_timed_out_requests_discard_late_results_without_clobbering_siblings() {
    let init_sources = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::SetSources {
            sources: vec![SourceDescriptor {
                id: "src".into(),
                name: "Source".into(),
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
    // Save Browse #1 (`folder_1`)'s dynamic `request_id` without answering yet,
    // answer Browse #2 (`folder_2`) echoing `{{REQ}}`, and replay Browse #1's saved ID on `emit_old`!
    let save_browse_one = format!(
        "{{{{SAVE_REQ}}}}{}",
        serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap()
    );
    let page_two = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ProvideSourcePage {
            request_id: Id(999_999_991),
            page: Box::new(SourcePage {
                source_id: "src".into(),
                folder_id: Some("folder_2".into()),
                parent_id: None,
                title: "Folder Two".into(),
                entries: vec![SourceEntry {
                    id: "v2".into(),
                    name: "Video Two".into(),
                    kind: iris_api::SourceEntryKind::Video,
                    size: None,
                    modified_secs: None,
                    duration: None,
                    subtitle: None,
                    variants: Vec::new(),
                    thumbnail: None,
                }],
                next_page: None,
                authenticated: true,
                auth_hint: None,
                filters: Vec::new(),
            }),
        }],
    })
    .unwrap()
    .replace("999999991", "{{REQ}}");
    let stale_page_one = serde_json::to_string(&Reply::Ok {
        ok: vec![HostCommand::ProvideSourcePage {
            request_id: Id(999_999_992),
            page: Box::new(SourcePage {
                source_id: "src".into(),
                folder_id: Some("folder_1".into()),
                parent_id: None,
                title: "Stale Folder One".into(),
                entries: Vec::new(),
                next_page: None,
                authenticated: true,
                auth_hint: None,
                filters: Vec::new(),
            }),
        }],
    })
    .unwrap()
    .replace("999999992", "{{SAVED_REQ}}");
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(
        &[
            (b"w_opened", &init_sources),
            (b"folder_1", &save_browse_one),
            (b"folder_2", &page_two),
            (b"emit_old", &stale_page_one),
        ],
        &empty_reply,
    );
    let zip = package_zip(
        "demo.race",
        &[Capability::MediaSource],
        &[CommandSpec {
            id: "emit_old".into(),
            title: "Emit Stale".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        None,
        &wat,
    );

    let mut rig = AppRig::new("cancel-race", Vec::new());
    rig.install_zip("demo.race", &zip);
    wait_until("source ready", || {
        let _ = rig.poll(Instant::now());
        !rig.ctrl.media_sources().is_empty()
    });

    let t0 = Instant::now();
    // 1. Start Browse #1 (`folder_1`), then supersede it with Browse #2 (`folder_2`).
    let (ok1, out1) = rig.ctrl.browse_source(
        &mut rig.active_source,
        "demo.race",
        "src",
        Some("folder_1".into()),
        None,
        None,
        t0,
    );
    assert!(ok1 && rig.ctrl.has_pending_browse());
    let req1 = out1
        .effects
        .iter()
        .find_map(|e| match e {
            PluginSideEffect::ScheduleBrowseTimeout { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("browse #1 timeout scheduled");

    // Supersede Browse #1 with Browse #2 (`folder_2`).
    let (ok2, _) = rig.ctrl.browse_source(
        &mut rig.active_source,
        "demo.race",
        "src",
        Some("folder_2".into()),
        None,
        None,
        t0,
    );
    assert!(ok2);
    wait_until("browse #2 accepted", || {
        let _ = rig.poll(t0 + Duration::from_millis(20));
        rig.ctrl.source_entries("src").count() == 1
    });

    // Late arrival or error for superseded Browse #1 (`req1`) must NOT clobber Folder Two!
    let (matched_old_err, _) =
        rig.ctrl
            .handle_source_error(&mut rig.active_source, "demo.race", req1, "late err".into());
    assert!(!matched_old_err);
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.race",
        "emit_old",
        t0 + Duration::from_millis(30),
    );
    let _ = rig.poll(t0 + Duration::from_millis(40));
    match &rig.active_source {
        ActiveSource::Plugin(ps) => {
            assert_eq!(ps.title, "Folder Two");
            assert_eq!(ps.error, None);
        }
        ActiveSource::Local => panic!("expected plugin active source"),
    }

    // 2. Start a Resolve request AND then close browse -> closing browse must NOT cancel in-flight Resolve!
    let resolve_out = rig.ctrl.resolve_entry(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.race",
        "src",
        "v2",
        Some("Video Two".into()),
        None,
        t0 + Duration::from_millis(50),
    );
    let resolve_id = resolve_out
        .effects
        .iter()
        .find_map(|e| match e {
            PluginSideEffect::ScheduleResolveTimeout { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("scheduled resolve timeout");
    let _ = rig.ctrl.close_browse(&mut rig.active_source);
    assert!(
        rig.ctrl.has_pending_resolve(),
        "closing browse must never cancel an in-flight media resolve"
    );

    // 3. Firing `on_source_request_timeout` cancels the resolve, emits `MediaLoadCancelled` + timeout toast,
    // and ignores a second duplicate timeout!
    let timeout_out =
        rig.ctrl
            .on_source_request_timeout(&mut rig.active_source, "demo.race", resolve_id);
    assert!(!rig.ctrl.has_pending_resolve());
    assert!(
        timeout_out
            .effects
            .contains(&PluginSideEffect::MediaLoadCancelled)
    );
    assert!(timeout_out.effects.iter().any(|e| matches!(
        e,
        PluginSideEffect::ShowToast { message, .. } if message.contains("数据源请求超时")
    )));
    let dup_timeout =
        rig.ctrl
            .on_source_request_timeout(&mut rig.active_source, "demo.race", resolve_id);
    assert!(dup_timeout.effects.is_empty());
    assert_eq!(SOURCE_TIMEOUT, Duration::from_secs(30));
}

#[test]
fn media_generation_fencing_drops_stale_actions_and_effects_while_seek_preserves_control_work() {
    let burst_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![
            HostCommand::TogglePlayback,
            HostCommand::SetTimelineMeta {
                media: Id(0),
                generation: Generation::default(),
                chapters: vec![ApiChapter {
                    start_ms: 0.into(),
                    title: "Stale Chapter".into(),
                }],
                heat: Vec::new(),
                skip: Vec::new(),
            },
        ],
    })
    .unwrap();
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(&[(b"fire_old", &burst_reply)], &empty_reply);
    let zip = package_zip(
        "demo.fence",
        &[Capability::ControlPlayback, Capability::MediaEnhance],
        &[CommandSpec {
            id: "fire_old".into(),
            title: "Fire".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        None,
        &wat,
    );

    let mut rig = AppRig::new("gen-fence", Vec::new());
    rig.install_zip("demo.fence", &zip);
    rig.player.ops.clear();

    // Dispatch command in media generation 0, wait until wake_rx confirms the guest queued its
    // reply in Inbox, then advance media to generation 1 BEFORE calling `rig.poll(...)`!
    while rig.wake_rx.try_recv().is_ok() {}
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.fence",
        "fire_old",
        Instant::now(),
    );
    wait_until("stale reply queued in inbox", || {
        rig.wake_rx.try_recv().is_ok()
    });
    let (advanced, _) = rig.ctrl.advance_media(&mut rig.player);
    assert!(advanced);
    assert_eq!(rig.ctrl.generation().media, Id(1));
    rig.player.ops.clear();

    let after_advance = rig.poll(Instant::now());
    assert!(
        !rig.player
            .ops
            .iter()
            .any(|op| matches!(op, RecordedOp::Execute(_) | RecordedOp::SetTimeline { .. })),
        "stale-generation player commands and timeline effects must never execute: {:?}",
        rig.player.ops
    );
    assert!(rig.ctrl.timeline_ranges().is_none());
    assert!(!after_advance.domain.timeline_changed);

    // Seeking advances inner overlay seek epoch and clears glyphs/wake without advancing media generation!
    let seek_before = rig.ctrl.window().overlay_generation();
    let seek_out = rig.ctrl.advance_seek(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        Instant::now(),
    );
    let seek_after = rig.ctrl.window().overlay_generation();
    assert_eq!(seek_after.media, seek_before.media);
    assert_eq!(seek_after.seek.0, seek_before.seek.0 + 1);
    assert_eq!(
        seek_out.effects,
        vec![
            PluginSideEffect::ClearAllGlyphs,
            PluginSideEffect::ClearOverlayWake,
        ]
    );
}

#[test]
fn poll_batch_executes_actions_and_effects_in_strict_deterministic_order() {
    // Emit an ordered multi-command batch:
    // 1. Player SetSpeed(1.5)
    // 2. StepQueue (+1)
    // 3. RefreshSource ("feed")
    // 4. ShowToast ("batch-done")
    // 5. SetTimelineMeta (immediate ReadyEffect::Timeline)
    let ordered_reply = serde_json::to_string(&Reply::Ok {
        ok: vec![
            HostCommand::SetSpeed { speed: 1.5 },
            HostCommand::OpenNext,
            HostCommand::RefreshSource {
                source_id: "feed".into(),
            },
            HostCommand::ShowToast {
                message: "batch-done".into(),
            },
            HostCommand::SetTimelineMeta {
                media: Id(0),
                generation: Generation::default(),
                chapters: vec![ApiChapter {
                    start_ms: 1_000.into(),
                    title: "Ch1".into(),
                }],
                heat: Vec::new(),
                skip: Vec::new(),
            },
        ],
    })
    .unwrap();
    let empty_reply = serde_json::to_string(&Reply::Ok::<HostCommand> { ok: Vec::new() }).unwrap();
    let wat = build_routed_wat(&[(b"run_ordr", &ordered_reply)], &empty_reply);
    let zip = package_zip(
        "demo.order",
        &[
            Capability::ControlPlayback,
            Capability::MediaSource,
            Capability::Notifications,
            Capability::MediaEnhance,
            Capability::ReadState,
        ],
        &[CommandSpec {
            id: "run_ordr".into(),
            title: "Run Ordered".into(),
            subtitle: None,
            shortcut_hint: None,
            context_menu: false,
        }],
        &[],
        None,
        &wat,
    );

    let mut rig = AppRig::new("batch-order", Vec::new());
    rig.install_zip("demo.order", &zip);
    rig.player.ops.clear();

    let t0 = Instant::now();
    // Verify 5Hz `maybe_tick` throttle (`< 200ms` skipped, `>= 200ms` dispatched).
    assert!(rig.ctrl.has_state_listeners());
    let early_tick = rig.ctrl.maybe_tick(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        t0,
    );
    assert!(early_tick.effects.is_empty());
    let _ = rig.ctrl.maybe_tick(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        t0 + Duration::from_millis(250),
    );

    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.order",
        "run_ordr",
        t0 + Duration::from_millis(260),
    );

    let mut captured_effects = Vec::new();
    wait_until("ordered batch drained", || {
        let batch = rig.poll(t0 + Duration::from_millis(280));
        if !batch.effects.is_empty() {
            captured_effects.extend(batch.effects);
        }
        rig.ctrl.timeline_ranges().is_some()
    });

    // Verify exact side-effect order: StepMedia -> RefreshSource -> ShowToast!
    assert_eq!(
        captured_effects,
        vec![
            PluginSideEffect::StepMedia {
                delta: 1,
                wrap: true,
            },
            PluginSideEffect::RefreshSource {
                plugin_id: "demo.order".into(),
                source_id: "feed".into(),
            },
            PluginSideEffect::ShowToast {
                message: "batch-done".into(),
                is_error: false,
            },
        ]
    );
    // And verify exact player operation order: SetSpeed(1.5) BEFORE SetTimeline!
    assert_eq!(rig.player.ops.len(), 2);
    assert!(matches!(&rig.player.ops[0], RecordedOp::Execute(s) if s.contains("SetSpeed")));
    assert!(matches!(
        &rig.player.ops[1],
        RecordedOp::SetTimeline { count: 1, .. }
    ));
}

#[test]
fn monitor_lut_artifact_workflow_delivers_filter_and_rolls_back_on_failure() {
    let encode = |s: &str| s.bytes().map(|b| format!("\\{b:02x}")).collect::<String>();
    let begin_json = r#"{"operation":"artifact_begin","format":"cube"}"#;
    let clear_json = r#"{"ok":[{"command":"set_monitor_lut","media":"0","generation":{"media":"0","seek":"0","context":"0"},"artifact":null}]}"#;
    let empty_ok = r#"{"ok":[]}"#;
    let cube1 = format!("LUT_3D_SIZE 2\n{}", "1 0 0\n".repeat(8));
    let write_prefix = r#"{"operation":"artifact_write","handle":""#;
    let write_mid = r#"","blob":""#;
    let write_suffix = r#"","finish":true}"#;
    let cmd_prefix = r#"{"ok":[{"command":"set_monitor_lut","media":"0","generation":{"media":"0","seek":"0","context":"0"},"artifact":""#;
    let cmd_suffix = r#""}]}"#;
    let wat = format!(
        r#"(module
            (import "iris_v2" "host_submit" (func $submit (param i32 i32) (result i64)))
            (import "iris_v2" "host_blob_new" (func $blob_new (param i32 i32) (result i64)))
            (memory (export "memory") 4 8)
            (data (i32.const 1024) "{begin}")
            (data (i32.const 1152) "{empty}")
            (data (i32.const 1280) "{c1}")
            (data (i32.const 1664) "{wp}")
            (data (i32.const 1792) "{wm}")
            (data (i32.const 1856) "{ws}")
            (data (i32.const 1920) "{cp}")
            (data (i32.const 2112) "{cs}")
            (data (i32.const 2240) "{clr}")
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
                (local $pos i32) (local $cur i32) (local $blob i64)
                local.get $ptr local.get $len i64.const {pat_lut1} call $has8 i32.const 0 i32.ge_s
                if i32.const 1024 i32.const {begin_len} call $submit drop i64.const {empty_ret} return end
                local.get $ptr local.get $len i64.const {pat_clr} call $has8 i32.const 0 i32.ge_s
                if i64.const {clr_ret} return end
                local.get $ptr local.get $len i64.const {pat_writer} call $has8 local.set $pos
                local.get $pos i32.const 0 i32.ge_s
                if
                    i32.const 1280 i32.const {cube_len} call $blob_new local.set $blob
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
        clr = encode(clear_json),
        clr_ret = (2240_u64 << 32) | clear_json.len() as u64,
        pat_lut1 = u64::from_le_bytes(*b"mk_lut_1"),
        pat_clr = u64::from_le_bytes(*b"clr_lut1"),
        pat_writer = u64::from_le_bytes(*b"writer\":"),
        pat_artifact = u64::from_le_bytes(*b"ifact\":\""),
    );
    let zip = package_zip(
        "demo.lutctrl",
        &[Capability::MediaEnhance],
        &[
            CommandSpec {
                id: "mk_lut_1".into(),
                title: "Set LUT".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
            CommandSpec {
                id: "clr_lut1".into(),
                title: "Clear LUT".into(),
                subtitle: None,
                shortcut_hint: None,
                context_menu: false,
            },
        ],
        &[],
        None,
        &wat,
    );
    let mut rig = AppRig::new("lut-ctrl", Vec::new());
    rig.install_zip("demo.lutctrl", &zip);

    // 1. Failure path: `set_monitor_lut` error does not commit LUT owner.
    rig.player.fail_lut = true;
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.lutctrl",
        "mk_lut_1",
        Instant::now(),
    );
    let mut saw_err = false;
    wait_until("lut failure surfaces toast", || {
        let batch = rig.poll(Instant::now());
        saw_err |= batch.effects.iter().any(|e| {
            matches!(
                e,
                PluginSideEffect::ShowToast { message, .. } if message.contains("LUT 未应用")
            )
        });
        saw_err
    });

    // 2. Success path: commits LUT owner and clears cleanly on `artifact: null`.
    rig.player.fail_lut = false;
    rig.player.ops.clear();
    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.lutctrl",
        "mk_lut_1",
        Instant::now(),
    );
    wait_until("lut applied", || {
        let _ = rig.poll(Instant::now());
        rig.player.ops.contains(&RecordedOp::SetLut(true))
    });

    let _ = rig.ctrl.invoke_command(
        PluginContext {
            player: &mut rig.player,
            store: &mut rig.store,
            settings: &rig.settings,
            active_source: &mut rig.active_source,
        },
        "demo.lutctrl",
        "clr_lut1",
        Instant::now(),
    );
    wait_until("lut cleared", || {
        let _ = rig.poll(Instant::now());
        rig.player.ops.contains(&RecordedOp::SetLut(false))
    });
}
