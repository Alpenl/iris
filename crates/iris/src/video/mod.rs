//! Platform video surfaces. Each one puts mpv's picture *under* the GPUI
//! window content, so GPUI can paint panels and overlays on top of it.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::VideoSurface;
