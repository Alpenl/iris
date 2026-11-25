use iris_core::player::AspectMode;
use iris_core::preset::{
    DEFAULT_WINDOW_SIZE, PresetCaptureScope, PresetLayout, PresetManager, PresetRuntime,
    SidebarMode,
};
use iris_core::settings::{Settings, Side};

#[test]
fn another_windows_layout_save_preserves_explicit_plugin_toggles() {
    let dir = std::env::temp_dir().join(format!("iris-preset-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("settings.conf");
    Settings::default().save_to(&file).unwrap();
    let manager = PresetManager::load_from(&dir.join("presets.json"));
    let (mut a, mut first, _) = PresetRuntime::restore_on_startup_with_path(
        &manager,
        Settings::load_from(&file),
        Some(file.clone()),
    );
    let (mut b, mut second, _) = PresetRuntime::restore_on_startup_with_path(
        &manager,
        Settings::load_from(&file),
        Some(file.clone()),
    );
    first.set_plugin_enabled(&mut a, "test.tool", false);
    b.directory_width = 333.;
    second.record_user_edit(&b);
    assert!(
        Settings::load_from(&file)
            .disabled_plugins
            .contains(&"test.tool".into())
    );
    // The second window's settings still say enabled. An explicit enable click
    // must nevertheless win, while a later unrelated first-window edit cannot undo it.
    second.set_plugin_enabled(&mut b, "test.tool", true);
    a.directory_width = 444.;
    first.record_user_edit(&a);
    assert!(
        !Settings::load_from(&file)
            .disabled_plugins
            .contains(&"test.tool".into())
    );
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn sparse_revert_user_edit_promotion_crash_restart_and_custom_crud() {
    let dir = std::env::temp_dir().join(format!("iris-preset-1b-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");

    let mut mgr = PresetManager::load_from(&presets_file);
    assert_eq!(mgr.list().len(), 5);
    assert!(!mgr.delete_custom("cinema"));

    // Built-in presets never touch external plugin enable/disable lists.
    for b in mgr.list() {
        if let Some(pl) = &b.plugins {
            assert!(pl.enable_plugins.is_empty());
            assert!(pl.disable_plugins.is_empty());
        }
    }

    // User's personal habit: 10s seek_step, 1.0x sub_scale, and disabled "ext.noise".
    let initial = Settings {
        seek_step: 10.0,
        sub_scale: 1.0,
        disabled_plugins: vec!["ext.noise".into()],
        ..Default::default()
    };
    initial.save_to(&settings_file).unwrap();

    let (mut active, mut rt, _) = PresetRuntime::restore_on_startup_with_path(
        &mgr,
        Settings::load_from(&settings_file),
        Some(settings_file.clone()),
    );
    assert_eq!(active.seek_step, 10.0);
    assert!(!rt.is_modified(&active));
    let layout = PresetLayout {
        sidebar_mode: SidebarMode::Queue,
        window_size: Some((1000., 800.)),
    };

    // 1. Switch to `study` (seek_step = 3.0, sub_scale = 1.25) -> then `pip` (seek_step = None):
    //    `seek_step` automatically reverts to user's habitual 10.0s!
    let tr_study = rt.switch_preset(&mgr, "study", &mut active, layout);
    assert_eq!(tr_study.sidebar_mode, Some(SidebarMode::Plugins));
    assert_eq!(active.seek_step, 3.0);
    assert_eq!(active.sub_scale, 1.25);
    assert_eq!(active.disabled_plugins, vec!["ext.noise"]);
    assert!(!rt.is_modified(&active));

    let tr_pip = rt.switch_preset(
        &mgr,
        "pip",
        &mut active,
        PresetLayout {
            sidebar_mode: tr_study.sidebar_mode.unwrap(),
            window_size: layout.window_size,
        },
    );
    assert_eq!(tr_pip.window_size, Some((560.0, 320.0)));
    active.window_width = Some(559.5);
    active.window_height = Some(320.5);
    rt.record_user_edit(&active);
    assert!(rt.baseline().window_width.is_none());
    assert!(rt.baseline().window_height.is_none());
    assert_eq!(
        active.seek_step, 10.0,
        "pip(None) must revert to 10s baseline"
    );
    assert_eq!(active.sub_scale, 1.0);

    // 2. Switch to `review` -> verify entry/exit commands for `iris.guides`.
    let tr_rev = rt.switch_preset(
        &mgr,
        "review",
        &mut active,
        PresetLayout {
            sidebar_mode: tr_pip.sidebar_mode.unwrap_or(layout.sidebar_mode),
            window_size: tr_pip.window_size,
        },
    );
    assert_eq!(tr_rev.window_size, layout.window_size);
    assert_eq!(
        tr_rev.plugin_commands,
        vec![("iris.guides".into(), "thirds".into())]
    );
    let tr_back_study = rt.switch_preset(&mgr, "study", &mut active, layout);
    assert_eq!(
        tr_back_study.on_exit_commands,
        vec![("iris.guides".into(), "off".into())]
    );
    assert_eq!(active.seek_step, 3.0);
    assert_eq!(active.sub_scale, 1.25);

    // 3. While in `study`, user manually changes `sub_scale` to 1.6 -> promotes into baseline!
    active.sub_scale = 1.6;
    rt.record_user_edit(&active);
    assert!(rt.is_modified(&active));
    assert_eq!(rt.baseline().sub_scale, 1.6);
    assert_eq!(rt.baseline().seek_step, 10.0);

    // 4. Simulate crash while in `study`: reload `settings.conf` from disk on cold startup.
    let disk_settings = Settings::load_from(&settings_file);
    assert_eq!(
        disk_settings.seek_step, 10.0,
        "on-disk baseline must stay 10s"
    );
    assert_eq!(
        disk_settings.sub_scale, 1.6,
        "promoted user edit must be on disk"
    );
    assert_eq!(disk_settings.active_preset, "study");

    let (mut restarted_active, mut restarted_rt, _) = PresetRuntime::restore_on_startup_with_path(
        &mgr,
        disk_settings,
        Some(settings_file.clone()),
    );
    assert_eq!(restarted_rt.baseline().seek_step, 10.0);
    assert_eq!(restarted_active.seek_step, 3.0, "study replayed in memory");

    let tr = restarted_rt.switch_preset(&mgr, "default", &mut restarted_active, layout);
    assert_eq!(tr.sidebar_mode, Some(SidebarMode::Files));
    assert_eq!(restarted_active.seek_step, 10.0);

    // 5. Custom preset capture, collision-safe slug generation, update, and deletion.
    restarted_active.directory_side = Side::Right;
    restarted_rt.set_plugin_enabled(&mut restarted_active, "ext.noise", true);
    restarted_rt.set_plugin_enabled(&mut restarted_active, "ext.distraction", false);
    restarted_rt.set_plugin_enabled(&mut restarted_active, "ext.temporary", false);
    restarted_rt.set_plugin_enabled(&mut restarted_active, "ext.temporary", true);
    assert_eq!(
        Settings::load_from(&settings_file).disabled_plugins,
        ["ext.distraction"],
        "GUI toggles persist immediately"
    );
    let captured = PresetManager::capture_from_current(
        "cinema",
        "我的剪辑视图",
        "右侧宽栏",
        PresetCaptureScope::default(),
        &restarted_active,
        &restarted_rt,
        SidebarMode::Queue,
        Some((1440.0, 900.0)),
        1.5,
        AspectMode::Ratio16x9,
    );
    let plugin_patch = captured.plugins.as_ref().unwrap();
    assert_eq!(plugin_patch.enable_plugins, ["ext.noise"]);
    assert_eq!(plugin_patch.disable_plugins, ["ext.distraction"]);
    let custom_id = mgr.save_custom(captured);
    assert_eq!(custom_id, "custom-1");
    assert!(mgr.update_from_current(
        "custom-1",
        PresetCaptureScope::default(),
        &restarted_active,
        &restarted_rt,
        SidebarMode::Sources,
        None,
        1.25,
        AspectMode::Original,
    ));
    let updated = mgr.get(&custom_id).unwrap().plugins.as_ref().unwrap();
    assert_eq!(updated.enable_plugins, ["ext.noise"]);
    assert_eq!(updated.disable_plugins, ["ext.distraction"]);

    restarted_rt.switch_preset(&mgr, &custom_id, &mut restarted_active, layout);
    let (mut active, mut rt, _) = PresetRuntime::restore_on_startup_with_path(
        &mgr,
        Settings::load_from(&settings_file),
        None,
    );
    // A reopened preset's explicit selections remain in its update even when the edits
    // were already committed to the personal baseline during the previous session.
    rt.set_plugin_enabled(&mut active, "ext.annoying", false);
    assert!(mgr.update_from_current(
        &custom_id,
        PresetCaptureScope::default(),
        &active,
        &rt,
        SidebarMode::Sources,
        None,
        1.25,
        AspectMode::Original,
    ));
    let updated = mgr.get(&custom_id).unwrap().plugins.as_ref().unwrap();
    assert_eq!(updated.enable_plugins, ["ext.noise"]);
    assert_eq!(updated.disable_plugins, ["ext.annoying", "ext.distraction"]);
    assert_eq!(mgr.next_id("pip"), "custom-1");
    assert_eq!(mgr.next_id("custom-1"), "default");
    assert!(mgr.delete_custom("custom-1"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_pip_restores_default_window_without_a_saved_size() {
    let dir = std::env::temp_dir().join(format!("iris-preset-startup-{}", std::process::id()));
    let mgr = PresetManager::load_from(&dir.join("presets.json"));
    let settings = Settings {
        active_preset: "pip".into(),
        ..Settings::default()
    };
    let (mut active, mut runtime, _) =
        PresetRuntime::restore_on_startup_with_path(&mgr, settings, None);
    let transition = runtime.switch_preset(&mgr, "default", &mut active, PresetLayout::default());
    assert_eq!(transition.window_size, Some(DEFAULT_WINDOW_SIZE));
    assert_eq!((active.window_width, active.window_height), (None, None));
}

#[test]
fn updating_active_or_inactive_preset_keeps_revert_and_plugin_scopes_isolated() {
    let dir = std::env::temp_dir().join(format!("iris-preset-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");

    let mut mgr = PresetManager::load_from(&presets_file);
    let initial = Settings {
        seek_step: 10.0,
        disabled_plugins: vec!["ext.alpha".into()],
        ..Default::default()
    };
    initial.save_to(&settings_file).unwrap();
    let (mut active, mut rt, _) = PresetRuntime::restore_on_startup_with_path(
        &mgr,
        Settings::load_from(&settings_file),
        Some(settings_file.clone()),
    );
    let layout = PresetLayout {
        sidebar_mode: SidebarMode::Files,
        window_size: Some((1280., 720.)),
    };

    // Save two custom presets: `p1` (forces `ext.alpha` disabled) and `p2` (clean plugin patch).
    let p1 = PresetManager::capture_from_current(
        "p1",
        "预设一",
        "",
        PresetCaptureScope::default(),
        &active,
        &rt,
        SidebarMode::Files,
        layout.window_size,
        1.0,
        AspectMode::Original,
    );
    let mut p1 = p1;
    p1.plugins.as_mut().unwrap().disable_plugins = vec!["ext.alpha".into()];
    mgr.save_custom(p1);
    mgr.save_custom(PresetManager::capture_from_current(
        "p2",
        "预设二",
        "",
        PresetCaptureScope::default(),
        &active,
        &rt,
        SidebarMode::Files,
        layout.window_size,
        1.0,
        AspectMode::Original,
    ));

    // Activate `p1`: `initial_plugins` holds `ext.alpha = true`, matching baseline.
    rt.switch_preset(&mgr, "p1", &mut active, layout);
    assert!(!rt.is_modified(&active));

    // Updating INACTIVE `p2` while `p1` is active must NOT copy `p1`'s `initial_plugins` into `p2`.
    assert!(mgr.update_from_current(
        "p2",
        PresetCaptureScope::default(),
        &active,
        &rt,
        SidebarMode::Files,
        layout.window_size,
        1.0,
        AspectMode::Original,
    ));
    assert!(
        mgr.get("p2")
            .unwrap()
            .plugins
            .as_ref()
            .unwrap()
            .disable_plugins
            .is_empty()
    );

    // Tweak `seek_step` and disable `ext.chat` while inside `p1` -> `is_modified` turns true.
    active.seek_step = 4.0;
    rt.record_user_edit(&active);
    rt.set_plugin_enabled(&mut active, "ext.chat", false);
    assert!(rt.is_modified(&active));

    // Merge live tweaks into active `p1` and sync runtime -> `is_modified` clears AND
    // switching back to `default` reverts `seek_step` (10.0) and `ext.chat` (enabled)!
    assert!(mgr.update_from_current(
        "p1",
        PresetCaptureScope::default(),
        &active,
        &rt,
        SidebarMode::Files,
        layout.window_size,
        1.0,
        AspectMode::Original,
    ));
    let updated_p1 = mgr.get("p1").unwrap().clone();
    rt.sync_updated_active_preset(&updated_p1, &active);
    assert!(!rt.is_modified(&active));
    assert_eq!(rt.baseline().seek_step, 10.0);
    assert!(
        !rt.baseline()
            .disabled_plugins
            .iter()
            .any(|x| x == "ext.chat")
    );

    rt.switch_preset(&mgr, "default", &mut active, layout);
    assert_eq!(active.seek_step, 10.0);
    assert_eq!(active.disabled_plugins, vec!["ext.alpha"]);

    let _ = std::fs::remove_dir_all(&dir);
}
