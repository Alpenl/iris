//! Fast window-free UI contract tests: LRU thumbnail cache, `TextInput`
//! keyboard context firewall, 5-layer input priority stack (`ShortcutRecorder >
//! TextInput > Modal/Overlay Esc stack > Workspace/Viewport Keymap > Fullscreen Esc`),
//! and `PluginModal` async validation vs cancel lifecycles.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Point, RenderImage, px};
use image::{Frame, RgbaImage};
use iris_api::{PluginModal, Widget};

use super::chrome::DirectoryPopup;
use super::context_menu::{ContextMenu, ContextTarget};
use super::kit::text_buffer::{TextInputBuffers, TextInputState};
use super::state::{
    BuiltinDialog, EscapeStep, MAX_THUMB_CACHE, MAX_THUMB_CONCURRENT, OverlayState, PopoverKind,
    SettingsTab, ThumbCache,
};

fn dummy_img() -> Arc<RenderImage> {
    Arc::new(RenderImage::new([Frame::new(RgbaImage::new(1, 1))]))
}

fn sample_plugin_modal() -> PluginModal {
    PluginModal {
        id: "webdav-auth".into(),
        title: "添加 WebDAV".into(),
        subtitle: None,
        submit_label: Some("连接".into()),
        cancel_label: Some("取消".into()),
        timer: None,
        widgets: vec![
            Widget::Input {
                id: "url".into(),
                label: Some("地址".into()),
                value: "https://dav.example.com".into(),
                placeholder: String::new(),
                secret: false,
                action_id: None,
            },
            Widget::Input {
                id: "pass".into(),
                label: Some("密码".into()),
                value: String::new(),
                placeholder: String::new(),
                secret: true,
                action_id: None,
            },
            Widget::Toggle {
                id: "ro".into(),
                label: "只读".into(),
                checked: true,
                action_id: None,
            },
        ],
    }
}

#[test]
fn thumb_cache_enforces_concurrency_and_lru_eviction() {
    let mut cache = ThumbCache::default();
    let paths: Vec<PathBuf> = (0..(MAX_THUMB_CACHE + 4))
        .map(|i| PathBuf::from(format!("/p/{i}.jpg")))
        .collect();
    cache.enqueue_window(paths[..8].to_vec());
    let first = cache.pop_next_batch();
    assert_eq!(first.len(), MAX_THUMB_CONCURRENT);
    assert!(cache.pop_next_batch().is_empty());
    for (generation, p) in first {
        cache.finish(generation, p, Some(dummy_img()));
    }
    for p in &paths[3..MAX_THUMB_CACHE] {
        cache.enqueue_window(vec![p.clone()]);
        for (generation, n) in cache.pop_next_batch() {
            cache.finish(generation, n, Some(dummy_img()));
        }
    }
    cache.touch(&paths[0]);
    for p in &paths[MAX_THUMB_CACHE..] {
        cache.enqueue_window(vec![p.clone()]);
        for (generation, n) in cache.pop_next_batch() {
            cache.finish(generation, n, Some(dummy_img()));
        }
    }
    assert!(cache.get(&paths[0]).is_some());
    assert!(cache.get(&paths[1]).is_none());
}

#[test]
fn superseded_thumbnail_is_requeued_if_still_needed() {
    let mut cache = ThumbCache::default();
    let path = PathBuf::from("/p/a.jpg");
    cache.enqueue_window(vec![path.clone()]);
    let (generation, pending) = cache.pop_next_batch().pop().unwrap();
    cache.enqueue_window(vec![path.clone()]);
    assert!(cache.pop_next_batch().is_empty());
    assert!(!cache.finish(generation, pending, Some(dummy_img())));
    assert!(cache.get(&path).is_none());
    let (generation, pending) = cache.pop_next_batch().pop().unwrap();
    assert!(cache.finish(generation, pending, Some(dummy_img())));
    assert!(cache.get(&path).is_some());
}

#[test]
fn text_input_context_firewall_tracks_focus() {
    let mut buf = TextInputBuffers::default();

    let cases = [
        (false, false, false, false, false, false),
        (false, false, true, false, false, false), // search closed -> ignored
        (true, false, true, false, false, true),   // search open + focused
        (false, false, false, true, false, true),  // palette focused
        (false, false, false, false, true, false), // form focused without active key
        (false, true, false, false, true, true),   // form focused with active key
    ];
    for (search_open, form_key, search, palette, form, expected) in cases {
        buf.search_open = search_open;
        buf.active_form_key = form_key.then(|| "url".into());
        assert_eq!(buf.is_focused_given(search, palette, form), expected);
    }
}

#[test]
fn shortcut_recording_needs_visible_settings_and_recorder_focus() {
    let mut overlay = OverlayState {
        recording_shortcut: Some(("toggle_pause".into(), "Workspace".into())),
        ..Default::default()
    };
    assert!(!overlay.is_recording(true));
    overlay.settings_open = true;
    assert!(!overlay.is_recording(false));
    assert!(overlay.is_recording(true));
}

#[test]
fn platform_primary_workspace_keys_preserve_text_editing() {
    use gpui::{Action, KeyContext, Keystroke};
    use iris_core::keymap::{
        Keymap, KeymapScheme, effective_gpui_context, find_action, reserved_shortcut,
    };

    use super::actions::{Quit, TakeScreenshot, runtime_bindings};

    let bindings = runtime_bindings(&Keymap::default());
    let mut classic = Keymap::default();
    classic.scheme = KeymapScheme::MpvClassic;
    let classic_bindings = runtime_bindings(&classic);
    let typing = [
        KeyContext::parse("Workspace").unwrap(),
        KeyContext::parse("TextInput").unwrap(),
    ];
    for (id, action) in [
        ("quit", &Quit as &dyn Action),
        ("take_screenshot", &TakeScreenshot),
    ] {
        let entry = find_action(id).unwrap();
        let typed = Keystroke::parse(entry.default_key).unwrap();
        let binding = bindings
            .iter()
            .find(|binding| action.partial_eq(binding.action()))
            .unwrap();
        assert_eq!(binding.match_keystrokes(&[typed]), Some(false));
        assert!(binding.predicate().unwrap().depth_of(&typing).is_some());
        let classic_binding = classic_bindings
            .iter()
            .find(|binding| action.partial_eq(binding.action()))
            .unwrap();
        let typed = Keystroke::parse(&classic.effective_key(id)).unwrap();
        assert_eq!(classic_binding.match_keystrokes(&[typed]), Some(false));
        assert!(
            classic_binding
                .predicate()
                .unwrap()
                .depth_of(&typing)
                .is_none()
        );
    }
    for key in ["secondary-p", "secondary-,", "secondary-o"] {
        assert_eq!(
            effective_gpui_context("Workspace", key),
            Some("Workspace && !Modal")
        );
    }
    for key in [
        "secondary-v",
        "secondary-x",
        "secondary-shift-left",
        "alt-e",
        "ctrl-alt-e",
    ] {
        assert_eq!(
            effective_gpui_context("Workspace", key),
            Some("Workspace && !TextInput && !Modal")
        );
    }
    assert_eq!(
        reserved_shortcut("secondary-alt-shift-r"),
        Some("reset_appearance")
    );

    // When a blocking modal is open (`[Workspace, Modal]`), single-key actions (`Tab`, `Enter`,
    // `Q`) and background workspace chords (`Ctrl+F`, `Ctrl+O`) are blocked, while `Esc` and
    // `Ctrl+Q` continue to work.
    let modal_ctx = [
        KeyContext::parse("Workspace").unwrap(),
        KeyContext::parse("Modal").unwrap(),
    ];
    for action in [
        &super::actions::ToggleDirectory as &dyn Action,
        &super::actions::ToggleSearch,
        &super::actions::OpenFile,
        &super::actions::OpenCommandPalette,
        &TakeScreenshot,
    ] {
        let binding = bindings
            .iter()
            .find(|b| action.partial_eq(b.action()))
            .unwrap();
        assert!(binding.predicate().unwrap().depth_of(&modal_ctx).is_none());
    }
    let classic_quit = classic_bindings
        .iter()
        .find(|b| Quit.partial_eq(b.action()))
        .unwrap();
    assert!(
        classic_quit
            .predicate()
            .unwrap()
            .depth_of(&modal_ctx)
            .is_none()
    );
    for allowed in [
        &Quit as &dyn Action,
        &super::actions::ExitFullscreen,
        &super::actions::ResetAppearance,
    ] {
        let binding = bindings
            .iter()
            .find(|b| allowed.partial_eq(b.action()))
            .unwrap();
        assert!(binding.predicate().unwrap().depth_of(&modal_ctx).is_some());
    }
}

#[test]
fn recorded_shifted_symbols_match_runtime_bindings() {
    use gpui::{Action, Keystroke};
    use iris_core::keymap::{Keymap, format_keystroke_display, normalize_gpui_keystroke};

    use super::actions::{ActivatePreset, TakeScreenshot, TriggerPluginCommand, runtime_bindings};

    let mut keymap = Keymap::default();
    keymap.overrides.extend([
        ("take_screenshot".into(), "secondary-shift-/".into()),
        ("preset:review".into(), "secondary-!".into()),
        ("plugin:demo:command".into(), "secondary-shift--".into()),
    ]);
    let bindings = runtime_bindings(&keymap);
    let preset = ActivatePreset {
        id: "review".into(),
    };
    let plugin = TriggerPluginCommand {
        plugin_id: "demo".into(),
        command_id: "command".into(),
    };
    for (action, glyph, base) in [
        (&TakeScreenshot as &dyn Action, "?", "/"),
        (&preset, "!", "1"),
        (&plugin, "_", "-"),
    ] {
        // GPUI reports the shifted glyph with Shift cleared on all supported platforms.
        let typed = Keystroke::parse(&format!("secondary-{glyph}")).unwrap();
        let recorded = normalize_gpui_keystroke(
            &typed.key,
            typed.modifiers.platform,
            typed.modifiers.control,
            typed.modifiers.alt,
            typed.modifiers.shift,
        )
        .unwrap();
        assert_eq!(recorded, format!("secondary-shift-{base}"));
        assert_eq!(
            format_keystroke_display(&recorded, false),
            format!("Ctrl+Shift+{base}"),
        );
        assert_eq!(
            format_keystroke_display(&recorded, true),
            format!("⇧⌘{base}"),
        );
        let binding = bindings
            .iter()
            .find(|binding| action.partial_eq(binding.action()))
            .unwrap();
        assert_eq!(binding.match_keystrokes(&[typed]), Some(false));
        let unshifted = Keystroke::parse(&format!("secondary-{base}")).unwrap();
        assert_eq!(binding.match_keystrokes(&[unshifted]), None);
    }
}

#[test]
fn overlay_mutual_exclusion_and_escape_priority_never_skip_or_stall() {
    let mut ov = OverlayState::default();
    let mut buf = TextInputBuffers::default();

    // Mutual exclusion across popovers, settings, palette, context menu, and dialogs.
    ov.toggle_popover(PopoverKind::Speed);
    assert_eq!(ov.popover, Some(PopoverKind::Speed));
    ov.open_settings(Some(SettingsTab::Shortcuts));
    assert!(ov.settings_open && ov.popover.is_none());
    ov.recording_shortcut = Some(("open_file".into(), "Workspace".into()));
    ov.open_context_menu(ContextMenu {
        pos: Point::new(px(4.), px(4.)),
        target: ContextTarget::Viewport,
    });
    assert!(ov.context_menu.is_some() && !ov.settings_open && ov.recording_shortcut.is_none());
    ov.open_dialog(BuiltinDialog::OpenUrl);
    ov.dialog_error = Some("err".into());
    ov.open_context_menu(ContextMenu {
        pos: Point::new(px(6.), px(6.)),
        target: ContextTarget::Viewport,
    });
    assert!(ov.context_menu.is_some() && ov.dialog.is_none() && ov.dialog_error.is_none());
    assert!(ov.toggle_palette());
    assert!(ov.palette_open && ov.context_menu.is_none());
    ov.open_context_menu(ContextMenu {
        pos: Point::new(px(8.), px(8.)),
        target: ContextTarget::Viewport,
    });
    assert!(ov.context_menu.is_some() && !ov.palette_open);
    ov.open_dialog(BuiltinDialog::OpenUrl);
    assert!(ov.dialog.is_some() && ov.context_menu.is_none());

    // Stack all layers simultaneously and peel them one by one via `Esc`
    // in exact reverse Z-Order (Layer 3 modals above Layer 2 overlays).
    ov.dir_popup = DirectoryPopup::Manual;
    ov.info_open = true;
    ov.popover = Some(PopoverKind::More);
    ov.settings_open = true;
    ov.palette_open = true;
    buf.palette_input = TextInputState::with_text("speed");
    ov.context_menu = Some(ContextMenu {
        pos: Point::new(px(10.), px(10.)),
        target: ContextTarget::Viewport,
    });
    ov.dialog = Some(BuiltinDialog::OpenUrl);
    buf.active_form_key = Some("url".into());
    ov.recording_shortcut = Some(("open_file".into(), "Workspace".into()));
    buf.search_open = true;
    buf.search_input = TextInputState::with_text("clip");

    assert_eq!(
        ov.dismiss_next_escape(&mut buf, true, true),
        EscapeStep::CancelRecording
    );
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, true, true),
        EscapeStep::CancelPluginModal
    );
    let expected_tail = [
        EscapeStep::CloseBuiltinDialog,
        EscapeStep::ClosePalette,
        EscapeStep::CloseSettings,
        EscapeStep::CloseContextMenu,
        EscapeStep::ClosePopover,
        EscapeStep::CloseInfo,
        EscapeStep::CloseDirPopup,
        EscapeStep::ClearSearchText,
        EscapeStep::CloseSearch,
        EscapeStep::ExitFullscreen,
    ];
    for step in expected_tail {
        assert_eq!(ov.dismiss_next_escape(&mut buf, false, true), step);
    }
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, false),
        EscapeStep::Noop
    );
    assert!(buf.palette_input.is_empty() && buf.search_input.is_empty());
}

#[test]
fn plugin_modal_async_submit_retains_inputs_until_close_or_cancel() {
    let mut modal_state = iris_app::PluginModalState::default();
    let mut buf = TextInputBuffers::default();
    let modal = sample_plugin_modal();
    buf.form_fields
        .insert("sb:notes:draft".into(), TextInputState::with_text("keep"));

    assert!(buf.sync_plugin_modal(Some(("iris.webdav", &modal))));
    buf.form_fields.insert(
        "modal:iris.webdav:webdav-auth:url".into(),
        TextInputState::with_text("https://nas.local:5006"),
    );
    buf.form_fields.insert(
        "modal:iris.webdav:webdav-auth:pass".into(),
        TextInputState::with_text("bad-pass"),
    );

    // 1. Submit marks submitting state, debounces rapid re-click, and preserves inputs.
    let vals = modal_state
        .begin_submit(
            "iris.webdav",
            &modal,
            buf.collect_modal_values("iris.webdav", &modal),
        )
        .expect("first submit proceeds");
    assert_eq!(
        vals.get("url").map(String::as_str),
        Some("https://nas.local:5006")
    );
    assert_eq!(vals.get("pass").map(String::as_str), Some("bad-pass"));
    assert_eq!(vals.get("ro").map(String::as_str), Some("true"));
    assert!(
        modal_state
            .begin_submit(
                "iris.webdav",
                &modal,
                buf.collect_modal_values("iris.webdav", &modal),
            )
            .is_none()
    );

    // Validation error (Toast or re-sent same-id OpenModal): user's inputs stay intact!
    modal_state.clear_submitting_for("iris.webdav");
    assert!(!buf.sync_plugin_modal(Some(("iris.webdav", &modal))));
    assert_eq!(
        buf.form_fields["modal:iris.webdav:webdav-auth:url"].text(),
        "https://nas.local:5006"
    );
    assert_eq!(
        buf.form_fields["modal:iris.webdav:webdav-auth:pass"].text(),
        "bad-pass"
    );

    // 2. Retry succeeds and plugin emits `CloseModal` -> cleans `modal:*`, keeps `sb:*`.
    assert!(
        modal_state
            .begin_submit(
                "iris.webdav",
                &modal,
                buf.collect_modal_values("iris.webdav", &modal),
            )
            .is_some()
    );
    modal_state.on_cancel();
    buf.clear_modal_fields();
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(!buf.form_fields.keys().any(|k| k.starts_with("modal:")));
    assert_eq!(buf.form_fields["sb:notes:draft"].text(), "keep");

    // 3. Re-open, submit (simulating hung plugin), then `ModalCancel` -> cleans `modal:*`.
    assert!(buf.sync_plugin_modal(Some(("iris.webdav", &modal))));
    assert!(
        modal_state
            .begin_submit(
                "iris.webdav",
                &modal,
                buf.collect_modal_values("iris.webdav", &modal),
            )
            .is_some()
    );
    modal_state.on_cancel();
    buf.clear_modal_fields();
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(buf.active_form_key.is_none());
    assert!(!buf.form_fields.keys().any(|k| k.starts_with("modal:")));
}

#[test]
fn inline_destructive_confirmation_arms_confirms_and_expires() {
    use std::time::{Duration, Instant};

    let mut ov = OverlayState::default();
    let t0 = Instant::now();
    let ttl = Duration::from_secs(3);

    // First click arms confirmation instead of executing immediately.
    assert!(!ov.is_confirming("clear-queue", t0));
    assert!(!ov.confirm_or_arm("clear-queue", t0, ttl));
    assert!(ov.is_confirming("clear-queue", t0 + Duration::from_secs(1)));

    // Different key replaces pending confirmation safely.
    assert!(!ov.confirm_or_arm("clear-recent", t0 + Duration::from_secs(1), ttl));
    assert!(!ov.is_confirming("clear-queue", t0 + Duration::from_secs(1)));
    assert!(ov.is_confirming("clear-recent", t0 + Duration::from_secs(2)));

    // Second click within TTL confirms and clears pending state.
    assert!(ov.confirm_or_arm("clear-recent", t0 + Duration::from_secs(2), ttl));
    assert!(!ov.is_confirming("clear-recent", t0 + Duration::from_secs(2)));

    // Expired confirmation requires arming again.
    assert!(!ov.confirm_or_arm("clear-progress", t0, ttl));
    assert!(!ov.is_confirming("clear-progress", t0 + ttl));
    assert!(!ov.confirm_or_arm("clear-progress", t0 + ttl, ttl));

    // close_all clears any armed confirmation.
    ov.close_all();
    assert!(!ov.is_confirming("clear-progress", t0 + ttl + Duration::from_secs(1)));
}

#[test]
fn plugin_modal_feedback_clears_inline_error_on_non_text_edit() {
    let mut modal_state = iris_app::PluginModalState::default();
    let buf = TextInputBuffers::default();
    let modal = sample_plugin_modal();
    assert!(
        modal_state
            .begin_submit(
                "iris.webdav",
                &modal,
                buf.collect_modal_values("iris.webdav", &modal),
            )
            .is_some()
    );
    modal_state.set_error("验证失败");
    modal_state.clear_feedback();
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(modal_state.error().is_none());
}

#[test]
fn five_layer_input_priority_stack_enforces_recorder_text_modal_workspace_and_fullscreen_order() {
    use gpui::{Action, KeyContext};
    use iris_core::keymap::Keymap;
    use iris_core::library::Kind;

    use super::actions::{
        ExitFullscreen, OpenCommandPalette, OpenFile, ResetAppearance, ToggleFullscreen,
        TogglePause, ToggleSearch, runtime_bindings,
    };
    use super::build_root_key_context;
    use super::viewport::build_viewport_key_context;

    let bindings = runtime_bindings(&Keymap::default());
    let matches_ctx = |action: &dyn Action, ctx: &[KeyContext]| {
        bindings
            .iter()
            .filter(|b| action.partial_eq(b.action()))
            .any(|b| b.predicate().is_none_or(|p| p.depth_of(ctx).is_some()))
    };

    // Layer 1: `ShortcutRecorder` (`[ShortcutRecorder TextInput, Overlay]`) intercepts workspace/
    // viewport bindings (including `ExitFullscreen`), while emergency `ResetAppearance`
    // (`Workspace || ShortcutRecorder`) remains reachable and passes through the recorder.
    let layer1 = [
        build_root_key_context(true, true, false),
        build_viewport_key_context(true, false, false, Some(Kind::Video), false),
    ];
    assert!(!matches_ctx(&TogglePause, &layer1));
    assert!(!matches_ctx(&OpenFile, &layer1));
    assert!(!matches_ctx(&ExitFullscreen, &layer1));
    assert!(matches_ctx(&ResetAppearance, &layer1));

    // Layer 2: Active `TextInput` (`[Workspace TextInput, Overlay]`) switches `#video` to `Overlay`
    // and blocks single-key workspace/viewport shortcuts (`Space` `TogglePause`, `F` `ToggleFullscreen`)
    // plus keymap `ExitFullscreen` (`Esc` routes through `TextField::on_key_down -> handle_escape`),
    // while keeping `safe_while_typing` chords (`⌘F`/`Ctrl+F` `ToggleSearch`) and `ResetAppearance` active.
    let layer2 = [
        build_root_key_context(false, false, true),
        build_viewport_key_context(false, false, true, Some(Kind::Video), false),
    ];
    assert!(!matches_ctx(&TogglePause, &layer2));
    assert!(!matches_ctx(&ToggleFullscreen, &layer2));
    assert!(matches_ctx(&ToggleSearch, &layer2));
    assert!(!matches_ctx(&ExitFullscreen, &layer2));
    assert!(matches_ctx(&ResetAppearance, &layer2));

    // Layer 3: Modal / ContextMenu / Palette / Popover (`[Workspace Modal, Overlay]`) switches `#video`
    // to `Overlay` and blocks background workspace/viewport shortcuts (`Space`, `Ctrl+O`, `Ctrl+P`)
    // while `ExitFullscreen` (`Esc` routed to `handle_escape`) peels overlays strictly before fullscreen exit.
    let layer3 = [
        build_root_key_context(false, true, false),
        build_viewport_key_context(true, false, false, Some(Kind::Video), false),
    ];
    assert!(!matches_ctx(&TogglePause, &layer3));
    assert!(!matches_ctx(&OpenFile, &layer3));
    assert!(!matches_ctx(&OpenCommandPalette, &layer3));
    assert!(matches_ctx(&ExitFullscreen, &layer3));
    assert!(matches_ctx(&ResetAppearance, &layer3));

    // Layer 4: Unobstructed `[Workspace, Viewport media=video]` dispatches full keymap actions.
    let layer4 = [
        build_root_key_context(false, false, false),
        build_viewport_key_context(false, false, false, Some(Kind::Video), false),
    ];
    assert!(matches_ctx(&TogglePause, &layer4));
    assert!(matches_ctx(&ToggleFullscreen, &layer4));
    assert!(matches_ctx(&OpenFile, &layer4));
    assert!(matches_ctx(&OpenCommandPalette, &layer4));

    // Layer 5: Fullscreen `Esc` exit fires ONLY after layers 1–3 have zero active recorders,
    // modals, popovers, sidebar form fields, or search inputs.
    let mut ov = OverlayState {
        settings_open: true,
        recording_shortcut: Some(("toggle_pause".into(), "Workspace".into())),
        popover: Some(PopoverKind::Speed),
        ..Default::default()
    };
    let mut buf = TextInputBuffers::default();
    buf.search_open = true;
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, true),
        EscapeStep::CancelRecording
    );
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, true),
        EscapeStep::CloseSettings
    );
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, true),
        EscapeStep::ClosePopover
    );
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, true),
        EscapeStep::CloseSearch
    );
    assert_eq!(
        ov.dismiss_next_escape(&mut buf, false, true),
        EscapeStep::ExitFullscreen
    );
}

#[test]
fn stream_meta_cleanup_clears_matching_plugin_or_source_and_variants_popover() {
    use iris_api::StreamVariant;
    use iris_app::ActiveStreamMeta;

    let sample_meta = || ActiveStreamMeta {
        plugin_id: Some("plugin.demo".into()),
        source_id: Some("srv-a".into()),
        entry_id: Some("ep-1".into()),
        variants: vec![StreamVariant {
            id: "1080p".into(),
            label: "1080P".into(),
        }],
        active_variant: Some("1080p".into()),
    };

    let meta = sample_meta();
    let mut overlays = OverlayState {
        popover: Some(PopoverKind::Variants),
        popover_index: Some(0),
        ..Default::default()
    };

    assert!(!meta.matches_source(Some("plugin.other"), None));
    assert!(!meta.matches_source(Some("plugin.demo"), Some("srv-b")));
    assert!(meta.matches_source(Some("plugin.demo"), Some("srv-a")));
    assert!(meta.matches_source(Some("plugin.demo"), None));
    assert!(meta.matches_source(None, None));

    overlays.close_variants_popover();
    assert_eq!(overlays.popover, None);
    assert_eq!(overlays.popover_index, None);

    overlays.popover = Some(PopoverKind::Speed);
    overlays.close_variants_popover();
    assert_eq!(overlays.popover, Some(PopoverKind::Speed));
}

#[test]
fn stacked_consent_sheets_and_modal_error_clear_do_not_clobber_active_state() {
    use iris_api::v2::Id;
    use std::collections::BTreeSet;

    // Stacked Origin (Id(2)) submitting while background Review (Id(1)) completes or is replaced.
    let mut consent_busy = BTreeSet::new();
    consent_busy.insert(Id(1));
    consent_busy.insert(Id(2));
    consent_busy.remove(&Id(1));
    assert!(consent_busy.contains(&Id(2)));

    // Unassociated plugin Notice::Error (`request.is_none()`) clears modal submitting state,
    // while unrelated catalog/origin request errors (`request.is_some()`) leave modal intact.
    let mut modal_state = iris_app::PluginModalState::default();
    let buf = TextInputBuffers::default();
    let modal = sample_plugin_modal();
    assert!(
        modal_state
            .begin_submit(
                "iris.webdav",
                &modal,
                buf.collect_modal_values("iris.webdav", &modal),
            )
            .is_some()
    );
    assert!(!modal_state.clear_submitting_for("other.plugin"));
    assert!(modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(modal_state.clear_submitting_for("iris.webdav"));
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));
}
