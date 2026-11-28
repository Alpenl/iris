//! Headless workbench vertical slice (`crates/iris-app/src/workbench/`).
//!
//! Single authoritative owner of [`Settings`], [`PresetManager`], [`PresetRuntime`],
//! and [`Keymap`], plus explicit `iris-base::net::http_client::doh` configuration
//! synchronization. Intentionally free of window-visibility flags and UI region names
//! (`DirtyRegions` mapping belongs exclusively to `crates/iris/src/ui/views.rs`).

mod presets;
mod settings_ops;
mod shortcuts;

use std::path::PathBuf;

use iris_base::net::http_client::doh;
use iris_core::keymap::Keymap;
use iris_core::preset::{DEFAULT_WINDOW_SIZE, PresetManager, PresetRuntime};
use iris_core::settings::{MAX_SPEED, MIN_SPEED, Settings};

pub use presets::{
    PresetDeleteStep, PresetSaveStep, PresetSwitchStep, apply_appearance_reset_defaults,
};
pub use settings_ops::{
    DOH_PRESET_CUSTOM, DOH_PRESET_DOMESTIC_FIRST, DOH_PRESET_DOMESTIC_ONLY,
    DOH_PRESET_INTERNATIONAL, doh_resolver_preset_for, doh_resolvers_for_preset,
};
pub use shortcuts::ShortcutRecordOutcome;

/// Pure domain change summary emitted by workbench settings, preset, and keymap commands.
///
/// Contains zero window-visibility assumptions and zero UI region identifiers;
/// `crates/iris/src/ui/views.rs` maps this onto `DirtyRegions` using live `UiVisibility`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkbenchDomainChanges {
    /// Theme ID, UI font scale, or plugin theme/chrome overrides changed (`ThemeTokens` global update).
    pub theme_changed: bool,
    /// Settings values rendered inside `SettingsPanel` changed.
    pub settings_changed: bool,
    /// Root layout geometry changed (`directory_open`, `directory_side`, `directory_width`, `floating_dock`).
    pub root_layout_changed: bool,
    /// Sidebar layout, mode, or sort order changed.
    pub sidebar_changed: bool,
    /// Control bar density, floating mode, hidden buttons, or `on_eof` loop state changed.
    pub dock_changed: bool,
    /// Titlebar metadata visibility, pinned badge, or active preset pill changed.
    pub titlebar_changed: bool,
    /// Viewport-affecting preference changed (e.g. `photo_filmstrip`).
    pub viewport_changed: bool,
    /// Keymap scheme or shortcut overrides changed (`ui::rebind_keys` required).
    pub keymap_changed: bool,
    /// Target native `always_on_top` window attribute when changed.
    pub always_on_top_changed: Option<bool>,
    /// Target window logical dimensions `(width, height)` requested by a preset switch.
    pub window_resize: Option<(f32, f32)>,
}

impl WorkbenchDomainChanges {
    pub const fn settings_only() -> Self {
        Self {
            settings_changed: true,
            ..Self::empty()
        }
    }

    pub const fn empty() -> Self {
        Self {
            theme_changed: false,
            settings_changed: false,
            root_layout_changed: false,
            sidebar_changed: false,
            dock_changed: false,
            titlebar_changed: false,
            viewport_changed: false,
            keymap_changed: false,
            always_on_top_changed: None,
            window_resize: None,
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.theme_changed |= other.theme_changed;
        self.settings_changed |= other.settings_changed;
        self.root_layout_changed |= other.root_layout_changed;
        self.sidebar_changed |= other.sidebar_changed;
        self.dock_changed |= other.dock_changed;
        self.titlebar_changed |= other.titlebar_changed;
        self.viewport_changed |= other.viewport_changed;
        self.keymap_changed |= other.keymap_changed;
        if other.always_on_top_changed.is_some() {
            self.always_on_top_changed = other.always_on_top_changed;
        }
        if other.window_resize.is_some() {
            self.window_resize = other.window_resize;
        }
    }
}

/// Synchronizes `fake_ip_doh_fallback` and `doh_resolvers` from `settings` into
/// [`iris_base::net::http_client::doh`] immediately.
pub fn sync_doh_settings(settings: &Settings) {
    doh::set_fallback_enabled(settings.fake_ip_doh_fallback);
    if let Err(error) = doh::set_resolvers(&settings.doh_resolvers) {
        log::warn!("applying configured DoH resolvers: {error:#}");
    }
}

/// Computes the initial window logical `(width, height)` from restored startup `settings`.
pub fn initial_window_size(settings: &Settings) -> (f32, f32) {
    let use_saved_size = settings.remember_window_size || settings.active_preset != "default";
    if use_saved_size {
        (
            settings
                .window_width
                .unwrap_or(DEFAULT_WINDOW_SIZE.0)
                .clamp(480., 7680.),
            settings
                .window_height
                .unwrap_or(DEFAULT_WINDOW_SIZE.1)
                .clamp(300., 4320.),
        )
    } else {
        DEFAULT_WINDOW_SIZE
    }
}

/// Pre-window startup workbench bundle loaded before opening `PluginHost` and the GPUI window.
pub struct StartupWorkbench {
    pub(crate) workbench: WorkbenchController,
    pub(crate) deferred_plugin_commands: Vec<(String, String)>,
}

impl StartupWorkbench {
    /// Loads `Keymap`, `PresetManager`, and `Settings` from default user config paths,
    /// restores the active preset over the clean personal baseline, and syncs DoH immediately.
    pub fn load() -> Self {
        let keymap = Keymap::load();
        let presets = PresetManager::load();
        let loaded_settings = Settings::load();
        Self::from_loaded(
            loaded_settings,
            presets,
            keymap,
            iris_core::settings::path(),
        )
    }

    /// Deterministic constructor using explicit backing paths and syncing `doh`.
    pub fn from_paths(settings_path: PathBuf, presets_path: PathBuf, keymap_path: PathBuf) -> Self {
        Self::from_paths_with_doh_sink(settings_path, presets_path, keymap_path, sync_doh_settings)
    }

    /// Deterministic constructor with a custom DoH sync sink (isolates parallel unit tests
    /// that do not exercise the process-wide `doh` singleton).
    pub fn from_paths_with_doh_sink(
        settings_path: PathBuf,
        presets_path: PathBuf,
        keymap_path: PathBuf,
        doh_sync: fn(&Settings),
    ) -> Self {
        let keymap = Keymap::load_from(&keymap_path);
        let presets = PresetManager::load_from(&presets_path);
        let loaded_settings = Settings::load_from(&settings_path);
        Self::from_loaded_with_sink(
            loaded_settings,
            presets,
            keymap,
            Some(settings_path),
            doh_sync,
        )
    }

    fn from_loaded(
        loaded_settings: Settings,
        presets: PresetManager,
        keymap: Keymap,
        settings_path: Option<PathBuf>,
    ) -> Self {
        Self::from_loaded_with_sink(
            loaded_settings,
            presets,
            keymap,
            settings_path,
            sync_doh_settings,
        )
    }

    fn from_loaded_with_sink(
        loaded_settings: Settings,
        presets: PresetManager,
        keymap: Keymap,
        settings_path: Option<PathBuf>,
        doh_sync: fn(&Settings),
    ) -> Self {
        let (active_settings, preset_rt, deferred_plugin_commands) =
            PresetRuntime::restore_on_startup_with_path(&presets, loaded_settings, settings_path);
        let workbench = WorkbenchController::with_doh_sink(
            active_settings,
            presets,
            preset_rt,
            keymap,
            doh_sync,
        );
        Self {
            workbench,
            deferred_plugin_commands,
        }
    }

    pub fn settings(&self) -> &Settings {
        self.workbench.settings()
    }

    pub fn keymap(&self) -> &Keymap {
        self.workbench.keymap()
    }

    pub fn initial_window_size(&self) -> (f32, f32) {
        initial_window_size(self.workbench.settings())
    }
}

/// Single authoritative owner of `Settings`, `PresetManager`, `PresetRuntime`, and `Keymap`.
pub struct WorkbenchController {
    pub(crate) settings: Settings,
    pub(crate) presets: PresetManager,
    pub(crate) preset_rt: PresetRuntime,
    pub(crate) keymap: Keymap,
    pub(crate) doh_sync: fn(&Settings),
}

impl WorkbenchController {
    pub fn new(
        settings: Settings,
        presets: PresetManager,
        preset_rt: PresetRuntime,
        keymap: Keymap,
    ) -> Self {
        Self::with_doh_sink(settings, presets, preset_rt, keymap, sync_doh_settings)
    }

    pub fn with_doh_sink(
        settings: Settings,
        presets: PresetManager,
        preset_rt: PresetRuntime,
        keymap: Keymap,
        doh_sync: fn(&Settings),
    ) -> Self {
        doh_sync(&settings);
        Self {
            settings,
            presets,
            preset_rt,
            keymap,
            doh_sync,
        }
    }

    pub(crate) fn apply_doh_sync(&self) {
        (self.doh_sync)(&self.settings);
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn baseline_settings(&self) -> &Settings {
        self.preset_rt.baseline()
    }

    pub fn presets(&self) -> &PresetManager {
        &self.presets
    }

    pub fn is_preset_modified(&self) -> bool {
        self.preset_rt.is_modified(&self.settings)
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Persists user-edited `volume`, `speed`, and optional windowed `(width, height)` into the
    /// clean personal baseline during graceful window shutdown.
    pub fn persist_shutdown_baseline(
        &mut self,
        volume: f64,
        speed: f64,
        window_size_to_save: Option<(f32, f32)>,
    ) {
        self.settings.volume = Some(volume.clamp(0., 100.));
        if self.settings.remember_speed {
            self.settings.speed = Some(speed.clamp(MIN_SPEED, MAX_SPEED));
        }
        if self.settings.remember_window_size
            && let Some((w, h)) = window_size_to_save
        {
            self.settings.window_width = Some(w);
            self.settings.window_height = Some(h);
        }
        self.preset_rt.record_user_edit(&self.settings);
    }
}

#[cfg(test)]
mod tests;
