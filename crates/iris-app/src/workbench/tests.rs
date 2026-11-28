//! Headless unit tests for [`super::WorkbenchController`], sparse preset switching,
//! personal-baseline (`settings.conf`) isolation, DoH configuration sync, and
//! shortcut recording / conflict detection / scheme switching.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use iris_base::net::http_client::doh;
use iris_core::keymap::{ConflictTarget, Keymap, KeymapScheme};
use iris_core::player::AspectMode;
use iris_core::preset::{PresetCaptureScope, PresetLayout, PresetManager, SidebarMode};
use iris_core::settings::{OnEof, Settings, Side};

use super::{
    DOH_PRESET_CUSTOM, DOH_PRESET_DOMESTIC_FIRST, DOH_PRESET_DOMESTIC_ONLY,
    DOH_PRESET_INTERNATIONAL, ShortcutRecordOutcome, StartupWorkbench, WorkbenchController,
    doh_resolver_preset_for, initial_window_size,
};

struct TempWorkbenchDir {
    path: PathBuf,
}

impl TempWorkbenchDir {
    fn new(tag: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iris-app-workbench-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp workbench dir");
        Self {
            path: iris_core::library::safe_resolve_path(&path),
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempWorkbenchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn preset_switching_custom_crud_and_appearance_reset_preserve_clean_personal_baseline() {
    let dir = TempWorkbenchDir::new("presets");
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");
    let keymap_file = dir.join("keymap.json");

    let personal = Settings {
        theme_id: "graphite".into(),
        ui_font_scale: 1.15,
        directory_side: Side::Left,
        directory_width: 300.0,
        directory_open: true,
        on_eof: OnEof::Pause,
        seek_step: 5.0,
        ..Settings::default()
    };
    personal.save_to(&settings_file).unwrap();

    let startup = StartupWorkbench::from_paths_with_doh_sink(
        settings_file.clone(),
        presets_file.clone(),
        keymap_file,
        |_| {},
    );
    assert_eq!(startup.initial_window_size(), (1440.0, 900.0));
    let mut wb = startup.workbench;
    assert!(!wb.is_preset_modified());

    let base_layout = PresetLayout {
        sidebar_mode: SidebarMode::Files,
        window_size: Some((1440.0, 900.0)),
    };

    // Cycle across all built-in presets (`cinema` -> `review` -> `study` -> `pip` -> `default`).
    let cinema = wb.switch_preset("cinema", base_layout).unwrap();
    assert!(cinema.changes.theme_changed);
    assert_eq!(wb.settings().theme_id, "oled");
    assert!(!wb.settings().directory_open);
    assert!(wb.settings().floating_dock);
    assert_eq!(wb.settings().on_eof, OnEof::Next);
    assert_eq!(
        wb.settings().ui_font_scale,
        1.15,
        "font scale stays personal"
    );

    // Disk `settings.conf` must NOT be polluted by `cinema` overrides!
    let disk_during_cinema = Settings::load_from(&settings_file);
    assert_eq!(disk_during_cinema.active_preset, "cinema");
    assert_eq!(disk_during_cinema.theme_id, "graphite");
    assert!(disk_during_cinema.directory_open);
    assert!(!disk_during_cinema.floating_dock);
    assert_eq!(disk_during_cinema.on_eof, OnEof::Pause);

    let review = wb.switch_preset("review", base_layout).unwrap();
    assert!(review.changes.theme_changed);
    assert_eq!(wb.settings().theme_id, "studio");
    assert_eq!(wb.settings().directory_side, Side::Right);
    assert_eq!(wb.settings().directory_width, 360.0);
    assert_eq!(wb.settings().on_eof, OnEof::Pause);

    let study = wb.switch_preset("study", base_layout).unwrap();
    assert_eq!(study.transition.sidebar_mode, Some(SidebarMode::Plugins));
    assert_eq!(
        wb.settings().theme_id,
        "graphite",
        "review theme rolled back"
    );
    assert_eq!(wb.settings().seek_step, 3.0);
    assert_eq!(wb.settings().sub_scale, 1.25);

    let pip = wb.switch_preset("pip", base_layout).unwrap();
    assert_eq!(pip.changes.window_resize, Some((560.0, 320.0)));
    assert_eq!(pip.changes.always_on_top_changed, Some(true));
    assert!(!wb.settings().hidden_dock_buttons.is_empty());

    let back = wb.switch_preset("default", base_layout).unwrap();
    assert_eq!(back.changes.window_resize, Some((1440.0, 900.0)));
    assert_eq!(wb.settings().theme_id, "graphite");
    assert_eq!(wb.settings().directory_side, Side::Left);
    assert_eq!(wb.settings().directory_width, 300.0);
    assert!(wb.settings().directory_open);
    assert!(!wb.settings().always_on_top);
    assert!(wb.settings().hidden_dock_buttons.is_empty());
    assert!(!wb.is_preset_modified());

    // User tweak inside an active preset marks `is_preset_modified` and updates personal baseline
    // only for the edited field while keeping unedited preset overrides out of `settings.conf`.
    wb.switch_preset("cinema", base_layout).unwrap();
    wb.set_seek_step(15.0);
    assert_eq!(Settings::load_from(&settings_file).seek_step, 15.0);
    assert_eq!(Settings::load_from(&settings_file).theme_id, "graphite");

    // Save custom preset, bind shortcut, update in-place, then delete.
    let saved = wb.capture_and_save_custom_preset(
        "  审片工作流  ".into(),
        "4K HDR".into(),
        PresetCaptureScope::default(),
        "自定义工作区",
        SidebarMode::Queue,
        Some((1600.0, 960.0)),
        1.25,
        AspectMode::Ratio16x9,
    );
    assert_eq!(saved.saved_name, "审片工作流");
    wb.switch_preset(&saved.saved_id, base_layout).unwrap();
    assert_eq!(
        wb.record_normalized_shortcut(
            &format!("preset:{}", saved.saved_id),
            "Workspace",
            "alt-9",
            false
        ),
        ShortcutRecordOutcome::Bound {
            display: "Alt+9".into(),
            changes: crate::workbench::WorkbenchDomainChanges {
                settings_changed: true,
                sidebar_changed: true,
                dock_changed: true,
                titlebar_changed: true,
                keymap_changed: true,
                ..crate::workbench::WorkbenchDomainChanges::empty()
            },
        }
    );
    wb.step_directory_width(40.0);
    assert!(wb.is_preset_modified());
    assert!(
        wb.update_custom_preset(
            &saved.saved_id,
            PresetCaptureScope::default(),
            SidebarMode::Queue,
            Some((1600.0, 960.0)),
            1.25,
            AspectMode::Ratio16x9,
        )
        .is_some()
    );
    assert!(!wb.is_preset_modified());

    let deleted = wb.delete_custom_preset(&saved.saved_id).unwrap();
    assert!(deleted.was_active);
    assert!(deleted.changes.keymap_changed);
    assert!(
        !wb.keymap()
            .is_overridden(&format!("preset:{}", saved.saved_id))
    );
    assert!(wb.switch_preset("nonexistent", base_layout).is_none());

    // `ResetAppearance` restores Graphite defaults and clears hidden buttons.
    wb.toggle_floating_dock();
    wb.toggle_compact_controls();
    wb.toggle_dock_button_visibility("loop");
    wb.set_theme_id("plugin:rogue.theme");
    let reset_changes = wb.finish_appearance_reset();
    assert!(reset_changes.theme_changed);
    assert_eq!(wb.settings().theme_id, "graphite");
    assert!(!wb.settings().floating_dock);
    assert!(!wb.settings().compact_controls);
    assert!(wb.settings().show_titlebar_meta);
    assert!(wb.settings().hidden_dock_buttons.is_empty());

    // `initial_window_size` honors `remember_window_size = false` unless a non-default preset is active.
    let mut no_rem = Settings {
        remember_window_size: false,
        window_width: Some(1280.0),
        window_height: Some(720.0),
        ..Settings::default()
    };
    assert_eq!(initial_window_size(&no_rem), (1440.0, 900.0));
    no_rem.active_preset = "pip".into();
    assert_eq!(initial_window_size(&no_rem), (1280.0, 720.0));
}

#[test]
fn doh_settings_sync_immediately_on_startup_and_live_edits() {
    let dir = TempWorkbenchDir::new("doh");
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");
    let keymap_file = dir.join("keymap.json");

    let custom = Settings {
        fake_ip_doh_fallback: true,
        doh_resolvers: vec!["1.1.1.1".into(), "8.8.8.8".into(), "9.9.9.9".into()],
        ..Settings::default()
    };
    custom.save_to(&settings_file).unwrap();

    let startup = StartupWorkbench::from_paths(settings_file.clone(), presets_file, keymap_file);
    let mut wb = startup.workbench;
    assert!(doh::fallback_enabled());
    assert_eq!(
        doh::active_resolvers(),
        vec![
            "1.1.1.1".to_string(),
            "8.8.8.8".to_string(),
            "9.9.9.9".to_string()
        ]
    );
    assert_eq!(
        doh_resolver_preset_for(&wb.settings().doh_resolvers),
        DOH_PRESET_INTERNATIONAL
    );

    wb.set_doh_resolver_preset(DOH_PRESET_DOMESTIC_ONLY)
        .unwrap();
    assert_eq!(
        doh::active_resolvers(),
        vec!["223.5.5.5".to_string(), "1.12.12.12".to_string()]
    );
    assert_eq!(
        Settings::load_from(&settings_file).doh_resolvers,
        vec!["223.5.5.5".to_string(), "1.12.12.12".to_string()]
    );

    let (enabled, _) = wb.toggle_fake_ip_doh_fallback();
    assert!(!enabled);
    assert!(!doh::fallback_enabled());
    assert!(!Settings::load_from(&settings_file).fake_ip_doh_fallback);

    assert!(wb.set_doh_resolver_preset(DOH_PRESET_CUSTOM).is_none());
    wb.set_doh_resolver_preset(DOH_PRESET_DOMESTIC_FIRST)
        .unwrap();
    assert_eq!(
        doh_resolver_preset_for(&["9.9.9.9".to_string()]),
        DOH_PRESET_CUSTOM
    );
}

#[test]
fn shortcut_recorder_enforces_reserved_chords_scope_conflicts_schemes_and_reset() {
    let dir = TempWorkbenchDir::new("shortcuts");
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");
    let keymap_file = dir.join("keymap.json");

    let presets = PresetManager::load_from(&presets_file);
    let (settings, preset_rt, _) = iris_core::preset::PresetRuntime::restore_on_startup_with_path(
        &presets,
        Settings::default(),
        Some(settings_file),
    );
    let mut wb = WorkbenchController::with_doh_sink(
        settings,
        presets,
        preset_rt,
        Keymap::load_from(&keymap_file),
        |_| {},
    );

    // 1. Escape cancels without mutating overrides; malformed chords are ignored.
    assert_eq!(
        wb.record_normalized_shortcut("toggle_pause", "Viewport", "escape", false),
        ShortcutRecordOutcome::Cancelled
    );
    assert_eq!(
        wb.record_normalized_shortcut("toggle_pause", "Viewport", "ctrl--a", false),
        ShortcutRecordOutcome::Ignored
    );

    // 2. Emergency `reset_appearance` shortcut passes through even while recorder is open.
    assert_eq!(
        wb.record_normalized_shortcut("toggle_pause", "Viewport", "secondary-alt-shift-r", true),
        ShortcutRecordOutcome::PassThroughReserved("reset_appearance")
    );

    // 3. Overlapping scope conflicts are rejected with structured `ConflictTarget`.
    assert_eq!(
        wb.record_normalized_shortcut("zoom_in", "Viewport && media == image", "left", false),
        ShortcutRecordOutcome::Conflict {
            display: "←".into(),
            target: ConflictTarget::Action("photo_prev"),
        }
    );
    assert_eq!(
        wb.record_normalized_shortcut("zoom_in", "Viewport", "secondary-i", true),
        ShortcutRecordOutcome::Conflict {
            display: "⌘I".into(),
            target: ConflictTarget::Reserved("toggle_info"),
        }
    );

    // 4. Valid custom binding persists to `keymap.json` and updates formatted key hints.
    let bound =
        wb.record_normalized_shortcut("plugin:iris.guides:thirds", "Workspace", "alt-g", true);
    assert!(matches!(
        bound,
        ShortcutRecordOutcome::Bound {
            ref display,
            ref changes
        } if display == "⌥G" && changes.keymap_changed
    ));
    assert_eq!(
        Keymap::load_from(&keymap_file).plugin_bindings(),
        vec![("iris.guides", "thirds", "alt-g")]
    );

    // 5. Scheme switch & reset update effective keys across macOS and Windows display formats.
    assert_eq!(
        wb.key_hint_string("open_stream_url", true).as_deref(),
        Some("⌘U")
    );
    assert_eq!(
        wb.key_hint_string("open_stream_url", false).as_deref(),
        Some("Ctrl+U")
    );
    let scheme_changes = wb.set_keymap_scheme(KeymapScheme::MpvClassic);
    assert!(scheme_changes.keymap_changed);
    assert_eq!(wb.keymap().scheme, KeymapScheme::MpvClassic);

    wb.reset_shortcut_binding("plugin:iris.guides:thirds");
    assert!(wb.keymap().plugin_bindings().is_empty());

    wb.record_normalized_shortcut("toggle_mute", "Viewport", "alt-m", false);
    assert!(!wb.keymap().overrides.is_empty());
    wb.reset_all_shortcuts();
    assert!(wb.keymap().overrides.is_empty());
}
