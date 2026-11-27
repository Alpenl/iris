//! Platform video surfaces. Each one puts mpv's picture *under* the GPUI
//! window content, so GPUI can paint panels and overlays on top of it.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use anyhow::Result;
use futures::channel::mpsc::UnboundedReceiver;
use iris_app::VideoAttachSlot;
use iris_core::player::VideoRecoveryHandle;
use iris_mpv::RenderHandle;

/// Notification emitted after a GPU surface reset and rebuild completes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoReset {
    /// True when the video worker captured the active video track and queued `restore_vid`.
    pub(crate) restored_vid: bool,
}

/// Shared contract implemented by both the macOS (`CAOpenGLLayer`) and Windows
/// (`D3D11` + `DirectComposition`) video surface backends so signature drift is
/// caught at compile time on either platform.
pub(super) trait VideoSurfaceBackend: Sized {
    fn attach(
        window: &gpui::Window,
        mpv: RenderHandle,
        recovery: VideoRecoveryHandle,
    ) -> Result<(Self, UnboundedReceiver<VideoReset>)>;

    fn request_redraw(&self);

    fn set_frame(&self, x: f64, y: f64, w: f64, h: f64);

    fn set_scale(&mut self, scale: f64);

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn debug_force_device_reset(&self) -> bool;
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
type PlatformVideoSurface = mac::MacVideoSurface;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
type PlatformVideoSurface = windows::WinVideoSurface;

struct SurfaceAdapter<B: VideoSurfaceBackend>(B);

impl<B: VideoSurfaceBackend> SurfaceAdapter<B> {
    fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        self.0.set_frame(x, y, w, h);
    }

    fn set_scale(&mut self, scale: f64) {
        self.0.set_scale(scale);
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn debug_force_device_reset(&self) -> bool {
        self.0.debug_force_device_reset()
    }
}

pub(crate) struct VideoSurface(SurfaceAdapter<PlatformVideoSurface>);

impl VideoSurface {
    pub(crate) fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        self.0.set_frame(x, y, w, h);
    }

    pub(crate) fn set_scale(&mut self, scale: f64) {
        self.0.set_scale(scale);
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub(crate) fn debug_force_device_reset(&self) -> bool {
        self.0.debug_force_device_reset()
    }
}

pub(crate) fn attach(
    window: &gpui::Window,
    slot: VideoAttachSlot<'_>,
) -> Result<(VideoSurface, UnboundedReceiver<VideoReset>)> {
    let session = slot.session();
    let (backend, rx) =
        PlatformVideoSurface::attach(window, session.render_handle(), session.video_recovery())?;
    Ok((VideoSurface(SurfaceAdapter(backend)), rx))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use anyhow::Result;
    use futures::channel::mpsc::{UnboundedReceiver, unbounded};
    use iris_core::player::VideoRecoveryHandle;
    use iris_mpv::RenderHandle;

    use super::{SurfaceAdapter, VideoReset, VideoSurfaceBackend};

    #[derive(Default)]
    struct MockBackend {
        redraws: Cell<u32>,
        frame: Cell<[f64; 4]>,
        scale: f64,
    }

    impl VideoSurfaceBackend for MockBackend {
        fn attach(
            _window: &gpui::Window,
            _mpv: RenderHandle,
            _recovery: VideoRecoveryHandle,
        ) -> Result<(Self, UnboundedReceiver<VideoReset>)> {
            Ok((Self::default(), unbounded().1))
        }

        fn request_redraw(&self) {
            self.redraws.set(self.redraws.get() + 1);
        }

        fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
            self.frame.set([x, y, w, h]);
            self.request_redraw();
        }

        fn set_scale(&mut self, scale: f64) {
            self.scale = scale;
            self.request_redraw();
        }

        #[cfg(any(debug_assertions, feature = "devtools"))]
        fn debug_force_device_reset(&self) -> bool {
            true
        }
    }

    #[test]
    fn video_surface_delegates_through_shared_backend_trait() {
        let mut surface = SurfaceAdapter(MockBackend::default());
        surface.set_frame(10.0, 20.0, 1280.0, 720.0);
        surface.set_scale(1.75);
        assert_eq!(surface.0.frame.get(), [10.0, 20.0, 1280.0, 720.0]);
        assert_eq!(surface.0.scale, 1.75);
        assert_eq!(surface.0.redraws.get(), 2);
        #[cfg(any(debug_assertions, feature = "devtools"))]
        assert!(surface.debug_force_device_reset());
    }
}
