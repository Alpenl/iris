//! Fast window-free UI contract tests: LRU thumbnail cache, `TextInput`
//! keyboard context firewall, `OverlayState` mutual exclusion & 11-layer `Esc`
//! dismissal priority, and `PluginModal` async validation vs cancel lifecycles.

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
    for (_, p) in first {
        cache.finish(p, Some(dummy_img()));
    }
    for p in &paths[3..MAX_THUMB_CACHE] {
        cache.enqueue_window(vec![p.clone()]);
        for (_, n) in cache.pop_next_batch() {
            cache.finish(n, Some(dummy_img()));
        }
    }
    cache.touch(&paths[0]);
    for p in &paths[MAX_THUMB_CACHE..] {
        cache.enqueue_window(vec![p.clone()]);
        for (_, n) in cache.pop_next_batch() {
            cache.finish(n, Some(dummy_img()));
        }
    }
    assert!(cache.get(&paths[0]).is_some());
    assert!(cache.get(&paths[1]).is_none());
}

#[test]
fn text_input_context_firewall_tracks_focus_and_shortcut_recorder() {
    let mut ov = OverlayState::default();
    let mut buf = TextInputBuffers::default();

    // (search_open, has_form_key, has_rec, search_foc, pal_foc, form_foc, expected)
    let cases = [
        (false, false, false, false, false, false, false),
        (false, false, false, true, false, false, false), // search closed -> ignored
        (true, false, false, true, false, false, true),   // search open + focused
        (false, false, false, false, true, false, true),  // palette focused
        (false, false, false, false, false, true, false), // form focused without active key
        (false, true, false, false, false, true, true),   // form focused with active key
        (false, false, true, false, false, false, true),  // shortcut recorder active
    ];
    for (s_open, f_key, rec, sf, pf, ff, exp) in cases {
        buf.search_open = s_open;
        buf.active_form_key = f_key.then(|| "url".into());
        ov.recording_shortcut = rec.then(|| ("toggle_pause".into(), "Workspace".into()));
        assert_eq!(ov.is_text_input_active(&buf, sf, pf, ff), exp);
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
    assert!(ov.toggle_palette());
    assert!(ov.palette_open && !ov.settings_open);
    ov.open_context_menu(ContextMenu {
        pos: Point::new(px(8.), px(8.)),
        target: ContextTarget::Viewport,
    });
    assert!(ov.context_menu.is_some() && !ov.palette_open);
    ov.open_dialog(BuiltinDialog::OpenUrl);
    assert!(ov.dialog.is_some() && ov.context_menu.is_none());

    // Stack all layers simultaneously and peel them one by one via `Esc`.
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
        EscapeStep::CloseContextMenu,
        EscapeStep::ClosePalette,
        EscapeStep::CloseSettings,
        EscapeStep::ClosePopover,
        EscapeStep::CloseInfo,
        EscapeStep::CloseDirPopup,
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
    let mut ov = OverlayState::default();
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
    let vals = ov
        .begin_plugin_modal_submit(&buf, "iris.webdav", &modal)
        .expect("first submit proceeds");
    assert_eq!(
        vals.get("url").map(String::as_str),
        Some("https://nas.local:5006")
    );
    assert_eq!(vals.get("pass").map(String::as_str), Some("bad-pass"));
    assert_eq!(vals.get("ro").map(String::as_str), Some("true"));
    assert!(
        ov.begin_plugin_modal_submit(&buf, "iris.webdav", &modal)
            .is_none()
    );

    // Validation error (Toast or re-sent same-id OpenModal): user's inputs stay intact!
    ov.clear_submitting_for_plugin("iris.webdav");
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
        ov.begin_plugin_modal_submit(&buf, "iris.webdav", &modal)
            .is_some()
    );
    ov.complete_plugin_modal_close(&mut buf, None);
    assert!(ov.submitting_plugin_modal.is_none());
    assert!(!buf.form_fields.keys().any(|k| k.starts_with("modal:")));
    assert_eq!(buf.form_fields["sb:notes:draft"].text(), "keep");

    // 3. Re-open, submit (simulating hung plugin), then `ModalCancel` -> cleans `modal:*`.
    assert!(buf.sync_plugin_modal(Some(("iris.webdav", &modal))));
    assert!(
        ov.begin_plugin_modal_submit(&buf, "iris.webdav", &modal)
            .is_some()
    );
    ov.cancel_plugin_modal_state(&mut buf);
    assert!(ov.submitting_plugin_modal.is_none());
    assert!(buf.active_form_key.is_none());
    assert!(!buf.form_fields.keys().any(|k| k.starts_with("modal:")));
}
