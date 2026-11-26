//! Platform video surfaces. Each one puts mpv's picture *under* the GPUI
//! window content, so GPUI can paint panels and overlays on top of it.

/// Notification emitted after a GPU surface reset and rebuild completes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoReset {
    /// True when the video worker captured the active video track and queued `restore_vid`.
    pub(crate) restored_vid: bool,
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub(crate) use mac::VideoSurface;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::VideoSurface;
