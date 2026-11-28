//! Unified headless [`AppModel`] facade (`crates/iris-app/src/model/`).
//!
//! Coordinates [`PlaybackOrchestrator`], [`CatalogController`], [`PluginController`],
//! and [`WorkbenchController`] behind non-public fields so external presentation crates
//! (`crates/iris`) can only read domain snapshots or invoke semantic domain commands.

mod catalog_ops;
mod debug;
mod playback_ops;
mod plugin_ops;
mod workbench_ops;

use std::time::Instant;

use futures::channel::mpsc::UnboundedReceiver;
use iris_core::keymap::Keymap;
use iris_core::player::{Command, Session, SessionUpdate};
use iris_core::preset::{PresetManager, SidebarMode};
use iris_core::settings::Settings;
use iris_plugin_host::PluginHost;

use crate::catalog::CatalogController;
use crate::playback::{
    PlaybackOrchestrator, PlaybackSessionPort, VideoAttachSlot, WindowShutdownCoordinator,
};
use crate::plugins::{PluginContext, PluginController, PluginPlayer};
use crate::workbench::{StartupWorkbench, WorkbenchController};

pub use catalog_ops::MediaOpenCommit;
#[cfg(any(debug_assertions, feature = "devtools"))]
pub use debug::{DebugStateSnapshot, DebugUiSnapshot, parse_debug_filter};
pub use workbench_ops::{
    PresetDeleteOutcome, PresetSaveOutcome, PresetSwitchOutcome, ResetAppearanceOutcome,
};

/// Unified headless application model for a single player window.
///
/// All internal sub-controllers are non-`pub` (`pub(crate)`), preventing presentation
/// layers (`crates/iris`) from directly reading or mutating internal fields or bypassing
/// cross-domain invariants (`PresetRuntime::record_user_edit`, DoH sync, load fencing,
/// and two-phase native shutdown ordering).
pub struct AppModel<S = Session> {
    pub(crate) playback: PlaybackOrchestrator<S>,
    pub(crate) catalog: CatalogController,
    pub(crate) plugins: PluginController,
    pub(crate) workbench: WorkbenchController,
}

impl AppModel<Session> {
    /// Opens a live window model from `session`, `plugin_host`, and restored `startup` state.
    pub fn open_window(
        session: Session,
        plugin_host: PluginHost,
        startup: StartupWorkbench,
        now: Instant,
    ) -> anyhow::Result<(
        Self,
        UnboundedReceiver<SessionUpdate>,
        UnboundedReceiver<()>,
    )> {
        let (playback, session_rx) = PlaybackOrchestrator::open_window(session, now)?;
        let StartupWorkbench {
            workbench,
            deferred_plugin_commands,
        } = startup;
        let (plugins, plugin_wake_rx) = PluginController::open_window(
            plugin_host,
            &workbench.settings.disabled_plugins,
            &workbench.settings.theme_id,
            deferred_plugin_commands,
            now,
        );
        let initial_sidebar_mode = workbench
            .presets
            .get(&workbench.settings.active_preset)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.sidebar_mode)
            .unwrap_or(SidebarMode::Files);
        let catalog = CatalogController::load(
            initial_sidebar_mode,
            workbench.settings.sort_by,
            workbench.settings.sort_order,
            now,
        );
        let mut model = Self::from_parts(playback, catalog, plugins, workbench);
        model.apply_startup_session_settings();
        Ok((model, session_rx, plugin_wake_rx))
    }

    /// Consumes the one-shot [`VideoAttachSlot`] so `crates/iris/src/ui/` can attach
    /// the platform `VideoSurface` exactly once during window startup.
    pub fn take_video_attach_slot(&mut self) -> Option<VideoAttachSlot<'_>> {
        self.playback.take_video_attach_slot()
    }
}

impl<S> AppModel<S> {
    /// Deterministic constructor assembling pre-built sub-controllers (used by `open_window`
    /// and headless unit/contract tests).
    pub fn from_parts(
        playback: PlaybackOrchestrator<S>,
        catalog: CatalogController,
        plugins: PluginController,
        workbench: WorkbenchController,
    ) -> Self {
        Self {
            playback,
            catalog,
            plugins,
            workbench,
        }
    }

    pub fn playback(&self) -> &PlaybackOrchestrator<S> {
        &self.playback
    }

    pub fn session(&self) -> &S {
        self.playback.session()
    }

    pub fn catalog(&self) -> &CatalogController {
        &self.catalog
    }

    pub fn plugins(&self) -> &PluginController {
        &self.plugins
    }

    pub fn workbench(&self) -> &WorkbenchController {
        &self.workbench
    }

    pub fn settings(&self) -> &Settings {
        self.workbench.settings()
    }

    pub fn presets(&self) -> &PresetManager {
        self.workbench.presets()
    }

    pub fn is_preset_modified(&self) -> bool {
        self.workbench.is_preset_modified()
    }

    pub fn keymap(&self) -> &Keymap {
        self.workbench.keymap()
    }
}

impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    /// Applies restored startup settings, audio output device, volume, and remembered speed
    /// onto the underlying playback session.
    pub fn apply_startup_session_settings(&mut self) {
        let settings = self.workbench.settings.clone();
        let session = self.playback.session_mut();
        PlaybackSessionPort::apply_settings(session, &settings);
        if settings.audio_device != "auto" && !settings.audio_device.is_empty() {
            let _ = PlaybackSessionPort::execute(
                session,
                Command::SetAudioDevice(settings.audio_device.clone()),
            );
        }
        if let Some(vol) = settings.volume {
            let _ = PlaybackSessionPort::execute(session, Command::SetVolume(vol));
        }
        if (settings.remember_speed || settings.active_preset != "default")
            && let Some(speed) = settings.speed
        {
            let _ = PlaybackSessionPort::execute(session, Command::SetSpeed(speed));
        }
    }

    /// Phase 1a of two-phase native window shutdown: stops `WindowSession` & `PluginHost`.
    pub fn shutdown_plugins(&self) {
        self.plugins.shutdown();
    }

    /// Phase 1b of two-phase native window shutdown: flushes `Store` playback progress
    /// synchronously and commits `volume`, `speed`, and `window_size` to `settings.conf`.
    pub fn persist_shutdown_state(
        &mut self,
        window_size_to_save: Option<(f32, f32)>,
        now: Instant,
    ) {
        let progress = PluginPlayer::progress(self.playback.session());
        self.catalog.flush_on_shutdown(
            progress,
            self.workbench.settings.resume_playback,
            self.plugins.scoped_released(),
            now,
        );
        let (volume, speed) = {
            let st = PlaybackSessionPort::state(self.playback.session());
            (st.volume, st.speed)
        };
        self.workbench
            .persist_shutdown_baseline(volume, speed, window_size_to_save);
    }

    /// Convenience wrapper running both Phase-1 pre-surface-drop steps (`shutdown_plugins` +
    /// `persist_shutdown_state`) before `VideoSurface` is dropped.
    pub fn shutdown_before_surface_drop(
        &mut self,
        window_size_to_save: Option<(f32, f32)>,
        now: Instant,
    ) {
        self.shutdown_plugins();
        self.persist_shutdown_state(window_size_to_save, now);
    }

    /// Phase 2 of two-phase native window shutdown (MUST run AFTER `VideoSurface` is dropped):
    /// stops active session relays/event pump and terminates `Session`.
    pub fn shutdown_after_surface_drop(&self) {
        self.playback.shutdown();
    }

    /// Executes the idempotent 6-step two-phase window shutdown sequence via `coordinator`,
    /// guaranteeing that `drop_video_surface_first` completes strictly BEFORE `Session::shutdown()`.
    pub fn shutdown_with_coordinator(
        &mut self,
        coordinator: &mut WindowShutdownCoordinator,
        window_size_to_save: Option<(f32, f32)>,
        now: Instant,
        disable_sleep_preventer: impl FnOnce(),
        release_media_keys: impl FnOnce(),
        drop_video_surface_first: impl FnOnce(),
    ) -> bool {
        let Self {
            playback,
            catalog,
            plugins,
            workbench,
        } = self;
        coordinator.run_once(
            disable_sleep_preventer,
            release_media_keys,
            || plugins.shutdown(),
            || {
                let progress = PluginPlayer::progress(playback.session());
                catalog.flush_on_shutdown(
                    progress,
                    workbench.settings.resume_playback,
                    plugins.scoped_released(),
                    now,
                );
                let st = PlaybackSessionPort::state(playback.session());
                workbench.persist_shutdown_baseline(st.volume, st.speed, window_size_to_save);
            },
            drop_video_surface_first,
            || playback.shutdown(),
        )
    }

    pub(super) fn with_plugin_ctx<R>(
        &mut self,
        f: impl FnOnce(&mut PluginController, PluginContext<'_, S>) -> R,
    ) -> R {
        let plugins = &mut self.plugins;
        let player = self.playback.session_mut();
        let settings = &self.workbench.settings;
        self.catalog.with_plugin_parts(|store, active_source| {
            f(
                plugins,
                PluginContext {
                    player,
                    store,
                    settings,
                    active_source,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests;
