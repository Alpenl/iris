//! Headless unit and cross-layer contract tests for [`super::AppModel`],
//! multi-domain preset/plugin/catalog/playback coordination, two-phase native
//! shutdown ordering, narrow `devtools` diagnostics, and migrated app contracts.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use iris_api::v2::{Id, source::PlaylistEntry};
use iris_api::{PluginModal, StreamVariant, Widget};
use iris_core::bookmarks::LocationStore;
use iris_core::library::{Kind, SortBy, SortOrder};
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    Chapter, Command, ExternalTrack, MediaStats, MonitorLutFilter, PlaybackSnapshotInput,
    PlaybackState, ScopedPlayback, SessionEvent, Track, TrackKind,
};
use iris_core::preset::{PresetCaptureScope, SidebarMode};
use iris_core::settings::{Hwdec, OnEof, Settings, SubLang};
use iris_core::store::Store;
use iris_core::vault::CredentialVault;
use iris_plugin_host::PluginHost;

use super::AppModel;
use crate::catalog::CatalogController;
use crate::playback::{PlaybackOrchestrator, PlaybackSessionPort, WindowShutdownCoordinator};
use crate::plugins::{
    ActiveStreamMeta, PlayerProgress, PluginController, PluginModalState, PluginPlayer,
    find_plugin_playlist_index, remote_search_query, resolve_playlist_current,
    should_record_progress, should_suppress_playback_error_toast,
};
use crate::workbench::StartupWorkbench;

struct TempAppDir {
    path: PathBuf,
}

impl TempAppDir {
    fn new(tag: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("iris-app-model-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp app dir");
        Self {
            path: iris_core::library::safe_resolve_path(&path),
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempAppDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct MockAppSession {
    state: PlaybackState,
    kind: Option<Kind>,
    local_path: Option<PathBuf>,
    title: Option<String>,
    private_identity: bool,
    has_scoped: bool,
    commands: Vec<String>,
    trace: Rc<RefCell<Vec<&'static str>>>,
    shutdown_count: Cell<usize>,
}

impl MockAppSession {
    fn new(local_path: PathBuf, trace: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            state: PlaybackState {
                loaded: true,
                duration: 240.0,
                time_pos: 64.5,
                volume: 72.0,
                speed: 1.5,
                subs_visible: true,
                hwdec: "auto-safe".into(),
                ..Default::default()
            },
            kind: Some(Kind::Video),
            local_path: Some(local_path),
            title: Some("Sample Clip".into()),
            private_identity: false,
            has_scoped: false,
            commands: Vec::new(),
            trace,
            shutdown_count: Cell::new(0),
        }
    }
}

impl PlaybackSessionPort for MockAppSession {
    type Update = SessionEvent;

    fn coalesce_updates(batch: Vec<SessionEvent>) -> Vec<SessionEvent> {
        batch
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn kind(&self) -> Option<Kind> {
        self.kind
    }

    fn current(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }

    fn current_uri(&self) -> Option<&str> {
        None
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        self.local_path.clone().map(MediaUri::Local)
    }

    fn current_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn may_share_current_identity(&self) -> bool {
        !self.private_identity
    }

    fn display_size(&self) -> (i64, i64) {
        (1920, 1080)
    }

    fn display_rotation(&self) -> u16 {
        0
    }

    fn display_flips(&self) -> (bool, bool) {
        (false, false)
    }

    fn tracks_of(&self, _kind: TrackKind) -> Vec<&Track> {
        Vec::new()
    }

    fn selected_track(&self, _kind: TrackKind) -> Option<&Track> {
        None
    }

    fn media_stats(&self) -> MediaStats {
        MediaStats::default()
    }

    fn refresh_dynamic_stats(&self, _stats: &mut MediaStats) {}

    fn apply_update<'a>(&mut self, update: &'a SessionEvent) -> Option<&'a SessionEvent> {
        Some(update)
    }

    fn flush_deferred(&mut self) {}

    fn execute(&mut self, command: Command) -> Result<()> {
        let tag = match &command {
            Command::SetSpeed(sp) => {
                self.state.speed = *sp;
                format!("SetSpeed({sp:.2})")
            }
            Command::SetAspect(asp) => {
                self.state.aspect = *asp;
                format!("SetAspect({asp:?})")
            }
            Command::SetVolume(vol) => {
                self.state.volume = *vol;
                format!("SetVolume({vol:.1})")
            }
            Command::SetAudioDevice(dev) => format!("SetAudioDevice({dev})"),
            _ => "Command".into(),
        };
        self.commands.push(tag);
        Ok(())
    }

    fn set_hwdec(&mut self, hwdec: Hwdec) {
        self.commands.push(format!("hwdec:{hwdec:?}"));
    }

    fn set_sub_lang(&mut self, sub_lang: SubLang) {
        self.commands.push(format!("slang:{sub_lang:?}"));
    }

    fn set_sub_scale(&mut self, scale: f64) {
        self.commands.push(format!("sub_scale:{scale:.2}"));
    }

    fn set_on_eof_loop(&mut self, on_eof: OnEof) {
        self.commands.push(format!("on_eof:{on_eof:?}"));
    }

    fn apply_settings(&mut self, _settings: &Settings) {
        self.commands.push("apply_settings".into());
    }

    fn recover_unloaded_after_gpu_reset(
        &mut self,
        _settings: &Settings,
        _zoom: f64,
        _pan: [f64; 2],
    ) {
    }

    fn shutdown(&self) {
        self.shutdown_count.set(self.shutdown_count.get() + 1);
        self.trace.borrow_mut().push("session_shutdown");
    }
}

impl PluginPlayer for MockAppSession {
    fn current_local_path(&self) -> Option<&Path> {
        self.local_path.as_deref()
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        self.local_path.clone().map(MediaUri::Local)
    }

    fn current_title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn may_share_current_identity(&self) -> bool {
        !self.private_identity
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn has_scoped_stream(&self) -> bool {
        self.has_scoped
    }

    fn playback_snapshot_input(&self) -> PlaybackSnapshotInput {
        PlaybackSnapshotInput {
            kind: self.kind,
            media_uri: self.local_path.clone().map(MediaUri::Local),
            current_title: self.title.clone(),
            position: self.state.time_pos,
            duration: self.state.duration,
            paused: self.state.paused,
            volume: self.state.volume,
            speed: self.state.speed,
            ..Default::default()
        }
    }

    fn progress(&self) -> Option<PlayerProgress> {
        self.local_path.clone().map(|path| PlayerProgress::Local {
            path,
            pos: self.state.time_pos,
            dur: self.state.duration,
            eof: self.state.ended,
        })
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        PlaybackSessionPort::execute(self, command)
    }

    fn open_scoped(
        &mut self,
        _scoped: Arc<ScopedPlayback>,
        _uri: MediaUri,
        _settings: &Settings,
        _resume: f64,
    ) -> Result<()> {
        self.has_scoped = true;
        Ok(())
    }

    fn set_private_identity(&mut self, private: bool) {
        self.private_identity = private;
    }

    fn stop(&mut self) -> Result<()> {
        self.state.loaded = false;
        Ok(())
    }

    fn release_scoped(&mut self) {
        self.has_scoped = false;
    }

    fn set_plugin_timeline(&mut self, _owner: String, _chapters: &[Chapter]) -> Result<()> {
        Ok(())
    }

    fn attach_plugin_track(&mut self, _track: Arc<dyn ExternalTrack>) -> Result<()> {
        Ok(())
    }

    fn revoke_plugin_tracks(&mut self, _owner: &str) {}

    fn set_monitor_lut(&mut self, _filter: Option<Arc<dyn MonitorLutFilter>>) -> Result<()> {
        Ok(())
    }

    fn flush_deferred(&mut self) {}
}

fn build_test_app_model(
    dir: &TempAppDir,
    trace: Rc<RefCell<Vec<&'static str>>>,
) -> (AppModel<MockAppSession>, PathBuf, PathBuf, PathBuf) {
    let now = Instant::now();
    let media_file = dir.join("episode-01.mp4");
    std::fs::write(&media_file, b"stub").unwrap();
    let settings_file = dir.join("settings.conf");
    let presets_file = dir.join("presets.json");
    let keymap_file = dir.join("keymap.json");
    let store_dir = dir.join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let locations_file = dir.join("locations.json");
    let plugins_dir = dir.join("plugins-v2");

    let startup = StartupWorkbench::from_paths_with_doh_sink(
        settings_file.clone(),
        presets_file,
        keymap_file,
        |_| {},
    );
    let host = PluginHost::open_in(
        &plugins_dir,
        CredentialVault::in_memory(),
        &startup.settings().disabled_plugins,
        false,
    )
    .unwrap();
    let (plugins, _wake_rx) = PluginController::open_window(
        host,
        &startup.settings().disabled_plugins,
        &startup.settings().theme_id,
        Vec::new(),
        now,
    );
    let store = Store::load_from_dir(&store_dir);
    let locations = LocationStore::load_from(&locations_file);
    let catalog = CatalogController::with_stores(
        locations,
        store,
        SidebarMode::Files,
        SortBy::Name,
        SortOrder::Asc,
        now,
    );
    let playback = PlaybackOrchestrator::new(MockAppSession::new(media_file.clone(), trace), now);
    let mut app = AppModel::from_parts(playback, catalog, plugins, startup.workbench);
    app.apply_startup_session_settings();
    (app, settings_file, store_dir, media_file)
}

#[test]
fn app_model_orchestrates_presets_settings_appearance_reset_and_two_phase_shutdown() {
    let dir = TempAppDir::new("e2e");
    let trace = Rc::new(RefCell::new(Vec::new()));
    let (mut app, settings_file, records_file, media_file) =
        build_test_app_model(&dir, Rc::clone(&trace));
    let now = Instant::now();

    // 1. Switch to `study` preset -> updates `CatalogController::sidebar_mode()` and session settings.
    let study = app
        .activate_preset("study", Some((1440.0, 900.0)), now)
        .expect("study preset exists");
    assert_eq!(study.preset_id, "study");
    assert_eq!(app.catalog().sidebar_mode(), SidebarMode::Plugins);
    assert_eq!(app.settings().seek_step, 3.0);
    assert_eq!(app.settings().sub_scale, 1.25);

    // 2. Save custom preset, verify active state, then delete active custom preset -> falls back to `default`.
    let saved = app.save_current_as_preset(
        "My Study".into(),
        "Custom".into(),
        PresetCaptureScope::default(),
        "自定义工作区",
        Some((1500.0, 920.0)),
        now,
    );
    assert_eq!(app.settings().active_preset, saved.saved_id);
    let deleted = app
        .delete_custom_preset(&saved.saved_id, Some((1500.0, 920.0)), now)
        .expect("custom preset deleted");
    assert!(deleted.fallback_switch.is_some());
    assert_eq!(app.settings().active_preset, "default");
    assert_eq!(app.catalog().sidebar_mode(), SidebarMode::Files);
    assert_eq!(
        app.session().state.speed,
        1.0,
        "deleting active custom preset rolls speed back to personal baseline"
    );
    assert!(
        app.execute_player_command(Command::SetSpeed(1.25))
            .error
            .is_none()
    );

    // 3.Playback & track settings forward to `PlaybackSessionPort` AND persist clean baseline.
    app.set_hwdec(Hwdec::Off);
    app.set_sub_lang(SubLang::En);
    app.step_sub_scale(0.1);
    app.set_on_eof(OnEof::Loop);
    let (audio_out, _) = app.set_audio_device("dac-usb".into());
    assert!(audio_out.error.is_none());
    let (sort_by, sort_ord, _) = app.cycle_sort();
    assert_eq!(
        (app.settings().sort_by, app.settings().sort_order),
        (sort_by, sort_ord)
    );
    let (_, now_fav) = app.toggle_current_favorite().unwrap();
    assert!(now_fav);

    // 4. `ResetAppearance` restores Graphite defaults and keeps `WindowSession` healthy.
    app.set_theme_id("oled");
    app.toggle_floating_dock();
    let reset = app.reset_appearance(now);
    assert!(reset.workbench.theme_changed);
    assert_eq!(app.settings().theme_id, "graphite");
    assert!(!app.settings().floating_dock);

    // 5. Devtools diagnostic snapshots reflect live domain state accurately.
    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
        let applied_filter = app.debug_apply_filter("favorites");
        let st_snap = app.debug_state_snapshot();
        let ui_snap = app.debug_ui_snapshot();
        assert_eq!(st_snap.filter, applied_filter);
        assert!(st_snap.favorite);
        assert_eq!(ui_snap.active_preset, "default");
        assert_eq!(ui_snap.theme_id, "graphite");
    }

    // 6. Two-phase shutdown coordinator enforces `VideoSurface` drop strictly BEFORE `Session::shutdown()`
    // while persisting `Store` progress and clean `settings.conf` baseline first.
    app.activate_preset("cinema", Some((1440.0, 900.0)), now)
        .unwrap();
    let mut coord = WindowShutdownCoordinator::default();
    let run_window_shutdown = |coord: &mut WindowShutdownCoordinator,
                               app: &mut AppModel<MockAppSession>| {
        app.shutdown_with_coordinator(
            coord,
            Some((1520.0, 940.0)),
            now,
            || trace.borrow_mut().push("sleep_preventer_off"),
            || trace.borrow_mut().push("media_keys_drop"),
            || trace.borrow_mut().push("video_surface_drop"),
        )
    };

    assert!(run_window_shutdown(&mut coord, &mut app));
    // Re-entrant quit / Drop invocations are strict no-ops.
    assert!(!run_window_shutdown(&mut coord, &mut app));
    assert!(
        app.plugins()
            .window()
            .review_package(dir.join("post-shutdown.irisplugin"))
            .is_err(),
        "WindowSession and PluginHost reject new requests after Phase-1 shutdown"
    );
    assert_eq!(app.session().shutdown_count.get(), 1);
    assert_eq!(
        *trace.borrow(),
        vec![
            "sleep_preventer_off",
            "media_keys_drop",
            "video_surface_drop",
            "session_shutdown",
        ]
    );

    // Verify `Store` flushed playback progress to `records.tsv` and `settings.conf` saved clean baseline!
    let reloaded_store = Store::load_from_dir(&records_file);
    assert!(reloaded_store.is_favorite(&MediaUri::Local(media_file.clone())));
    assert!(reloaded_store.resume_path(&media_file) > 60.0);

    let disk_settings = Settings::load_from(&settings_file);
    assert_eq!(disk_settings.active_preset, "cinema");
    assert_eq!(
        disk_settings.theme_id, "graphite",
        "cinema oled not leaked to baseline"
    );
    assert_eq!(disk_settings.volume, Some(72.0));
    assert_eq!(disk_settings.speed, Some(1.25));
    assert_eq!(disk_settings.window_width, Some(1520.0));
    assert_eq!(disk_settings.window_height, Some(940.0));
}

fn sample_plugin_modal() -> PluginModal {
    PluginModal {
        id: "webdav-auth".into(),
        title: "连接 WebDAV".into(),
        subtitle: None,
        submit_label: Some("保存".into()),
        cancel_label: Some("取消".into()),
        timer: None,
        widgets: vec![
            Widget::Input {
                id: "url".into(),
                label: Some("服务器地址".into()),
                value: "https://example.com/dav".into(),
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
fn plugin_modal_async_submit_debounces_and_clears_on_error_or_cancel() {
    let mut modal_state = PluginModalState::default();
    let modal = sample_plugin_modal();
    let mut raw = BTreeMap::new();
    raw.insert("url".into(), "https://nas.local:5006".into());
    raw.insert("pass".into(), "bad-pass".into());
    raw.insert("ro".into(), "true".into());

    let vals = modal_state
        .begin_submit("iris.webdav", &modal, raw.clone())
        .expect("first submit proceeds");
    assert_eq!(
        vals.get("url").map(String::as_str),
        Some("https://nas.local:5006")
    );
    assert!(
        modal_state
            .begin_submit("iris.webdav", &modal, raw.clone())
            .is_none(),
        "duplicate submit while in-flight must be debounced"
    );

    modal_state.clear_submitting_for("iris.webdav");
    assert!(
        modal_state
            .begin_submit("iris.webdav", &modal, raw)
            .is_some(),
        "retry proceeds after validation error clears in-flight state"
    );
    modal_state.set_error("验证失败");
    modal_state.clear_feedback();
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(modal_state.error().is_none());

    modal_state.on_cancel();
    assert!(modal_state.take_focus_pending());
}

#[test]
fn stream_meta_playlist_search_scoped_guards_and_consent_isolation_contracts() {
    // 1. ActiveStreamMeta source/plugin matching
    let meta = ActiveStreamMeta {
        plugin_id: Some("plugin.demo".into()),
        source_id: Some("srv-a".into()),
        entry_id: Some("ep-1".into()),
        variants: vec![StreamVariant {
            id: "1080p".into(),
            label: "1080P".into(),
        }],
        active_variant: Some("1080p".into()),
    };
    assert!(!meta.matches_source(Some("plugin.other"), None));
    assert!(!meta.matches_source(Some("plugin.demo"), Some("srv-b")));
    assert!(meta.matches_source(Some("plugin.demo"), Some("srv-a")));
    assert!(meta.matches_source(Some("plugin.demo"), None));
    assert!(meta.matches_source(None, None));

    // 2. Plugin playlist resolution & stepping (cross-platform temp path)
    let playlist = vec![
        PlaylistEntry {
            source_id: "recommend".into(),
            entry_id: "ugc:BV1xx:101".into(),
            title: "P1".into(),
        },
        PlaylistEntry {
            source_id: "recommend".into(),
            entry_id: "ugc:BV1xx:102".into(),
            title: "P2".into(),
        },
        PlaylistEntry {
            source_id: "recommend".into(),
            entry_id: "ugc:BV1xx:103".into(),
            title: "P3".into(),
        },
    ];
    let private_uri = MediaUri::HttpUrl("https://plugin.invalid/private".into());
    let cur_p2 =
        resolve_playlist_current(&playlist, Some("recommend"), Some("ugc:BV1xx:102"), None);
    assert_eq!(
        find_plugin_playlist_index(
            &playlist,
            "community.bilibili",
            Some("community.bilibili"),
            cur_p2.as_ref(),
            Some(&private_uri),
        ),
        Some(1)
    );
    let parent_uri = MediaUri::Plugin {
        plugin_id: "community.bilibili".into(),
        source_id: "recommend".into(),
        entry_id: "BV1xx".into(),
    };
    let cur_p1 = resolve_playlist_current(&playlist, None, None, Some(("recommend", "BV1xx")));
    assert_eq!(
        find_plugin_playlist_index(
            &playlist,
            "community.bilibili",
            Some("community.bilibili"),
            cur_p1.as_ref(),
            Some(&parent_uri),
        ),
        Some(0)
    );
    let local_uri = MediaUri::Local(std::env::temp_dir().join("movie.mp4"));
    assert_eq!(
        find_plugin_playlist_index(
            &playlist,
            "community.bilibili",
            None,
            cur_p1.as_ref(),
            Some(&local_uri),
        ),
        None
    );

    // 3. Remote search query & scoped-release guards
    assert_eq!(remote_search_query(false, "anime"), None);
    assert_eq!(remote_search_query(true, "   \t\n "), None);
    assert_eq!(
        remote_search_query(true, "  eva 1080p "),
        Some("eva 1080p".into())
    );
    assert!(!should_suppress_playback_error_toast(false, false));
    assert!(should_suppress_playback_error_toast(true, false));
    assert!(should_suppress_playback_error_toast(false, true));
    assert!(should_record_progress(true, false));
    assert!(!should_record_progress(false, false));
    assert!(!should_record_progress(true, true));

    // 4. Stacked consent busy tracking & owner-scoped modal submitting clear
    let mut consent_busy = BTreeSet::new();
    consent_busy.insert(Id(1));
    consent_busy.insert(Id(2));
    consent_busy.remove(&Id(1));
    assert!(consent_busy.contains(&Id(2)));

    let mut modal_state = PluginModalState::default();
    let modal = sample_plugin_modal();
    assert!(
        modal_state
            .begin_submit("iris.webdav", &modal, BTreeMap::new())
            .is_some()
    );
    assert!(!modal_state.clear_submitting_for("other.plugin"));
    assert!(modal_state.is_submitting("iris.webdav", "webdav-auth"));
    assert!(modal_state.clear_submitting_for("iris.webdav"));
    assert!(!modal_state.is_submitting("iris.webdav", "webdav-auth"));

    // 5. GPU reset recovery generation fencing & restored-vid skip
    assert!(crate::should_recover_after_gpu_reset(
        4,
        4,
        Some(Kind::Photo),
        true,
        true
    ));
    assert!(!crate::should_recover_after_gpu_reset(
        4,
        4,
        Some(Kind::Video),
        true,
        true
    ));
    assert!(crate::should_recover_after_gpu_reset(
        4,
        4,
        Some(Kind::Video),
        false,
        true
    ));
    assert!(crate::should_recover_after_gpu_reset(
        4,
        4,
        Some(Kind::Video),
        true,
        false
    ));
    assert!(!crate::should_recover_after_gpu_reset(
        4,
        5,
        Some(Kind::Video),
        false,
        false
    ));
}
