//! Narrow window-initialization `VideoAttachSlot` token and idempotent two-phase
//! `VideoSurface -> Session::shutdown()` teardown coordinator.

use iris_core::player::Session;

/// Narrow one-shot binding token issued exclusively during window initialization
/// by [`super::PlaybackController::take_video_attach_slot`].
///
/// Allows `crates/iris/src/video::attach` (the sole `clippy::disallowed_*` exemption
/// in `crates/iris`) to bind native `mpv_render_context` and `VideoRecoveryHandle`
/// without `iris-app` ever calling `Session::render_handle` or `Session::video_recovery`.
pub struct VideoAttachSlot<'a> {
    session: &'a Session,
}

impl<'a> VideoAttachSlot<'a> {
    pub(super) const fn new(session: &'a Session) -> Self {
        Self { session }
    }

    pub const fn session(&self) -> &'a Session {
        self.session
    }
}

/// Explicit idempotent coordinator enforcing the cross-layer native shutdown invariant.
///
/// ### Two-Phase Native Teardown Invariant (`#26` Principle #3 / `#36`)
/// Normal window close (`window.on_window_should_close` / custom close button),
/// application quit (`Cmd+Q` / `Alt+F4` / `cx.on_app_quit`), and `Drop` fallback
/// MUST all route through this single coordinator (`done` guard guarantees once-only
/// execution without relying on struct field declaration order):
/// 1. Disable `SleepPreventer` (`IOPMAssertionRelease` / `SetThreadExecutionState`).
/// 2. Release `MediaKeyBridge` (`MPRemoteCommandCenter` / `SystemMediaTransportControls`).
/// 3. Shut down `WindowSession` and `PluginHost` (`plugins.shutdown()`).
/// 4. Flush/persist user playback progress and window/player settings.
/// 5. **Phase 1 (UI surface)**: Take and drop `VideoSurface` FIRST so `mpv_render_context_free`
///    completes and macOS OpenGL / Windows D3D11 render threads exit cleanly.
/// 6. **Phase 2 (Player session)**: Run `Session::shutdown()` SECOND (`quit` + event-pump join),
///    preventing quit/join from interleaving with an active render thread and stalling exit.
#[derive(Debug, Default)]
pub struct WindowShutdownCoordinator {
    done: bool,
}

impl WindowShutdownCoordinator {
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Executes the 6-step window shutdown sequence at most once, guaranteeing that
    /// `drop_video_surface_first` completes strictly before `shutdown_session_second` starts.
    pub fn run_once(
        &mut self,
        disable_sleep_preventer: impl FnOnce(),
        release_media_keys: impl FnOnce(),
        shutdown_plugins_and_host: impl FnOnce(),
        persist_state: impl FnOnce(),
        drop_video_surface_first: impl FnOnce(),
        shutdown_session_second: impl FnOnce(),
    ) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        disable_sleep_preventer();
        release_media_keys();
        shutdown_plugins_and_host();
        persist_state();
        // Phase 1: destroy `VideoSurface` (`mpv_render_context` + native render worker) FIRST.
        drop_video_surface_first();
        // Phase 2: shut down `Session` (`mpv` quit + event-pump thread join) SECOND.
        shutdown_session_second();
        true
    }
}
