//! Direction C "Modular panels" tokens.
//!
//! Detail rules follow Zed and Sooty: ghost buttons with no frame at rest,
//! one filled step for hover and another for selected, list rows without
//! separators, and text in three weights of grey.

use gpui::{Rgba, rgb, rgba};

pub const FONT_UI: &str = "Space Grotesk";
pub const FONT_MONO: &str = "IBM Plex Mono";

/// Title bar, control bar and the frame around the picture.
pub fn window_bg() -> Rgba {
    rgb(0x0E0F11)
}
/// Panels that hold content: the directory, popups, tooltips.
pub fn panel() -> Rgba {
    rgb(0x15171A)
}
/// Hovered buttons and rows.
pub fn hover() -> Rgba {
    rgb(0x1F2226)
}
/// The current row, pressed buttons and switched-on toggles.
pub fn selected() -> Rgba {
    rgb(0x25292E)
}
pub fn line() -> Rgba {
    rgb(0x2A2E33)
}
/// Borders and tracks that must read over the darker backgrounds.
pub fn line_strong() -> Rgba {
    rgb(0x3A3F46)
}
pub fn text() -> Rgba {
    rgb(0xE6E8EB)
}
/// File names in the directory: readable, quieter than the current one.
pub fn text_muted() -> Rgba {
    rgb(0xB4B9BF)
}
pub fn text_dim() -> Rgba {
    rgb(0x8D939A)
}
/// Icons at rest in lists, counts, separators.
pub fn text_faint() -> Rgba {
    rgb(0x5C6168)
}
pub fn accent() -> Rgba {
    rgb(0xFF5B1F)
}
/// Panel colour at 90% for overlays drawn on top of the video.
pub fn overlay() -> Rgba {
    rgba(0x15171AE6)
}
/// Dim backdrop behind centered modal panels.
pub fn scrim() -> Rgba {
    rgba(0x00000099)
}

pub const TITLEBAR_H: f32 = 36.;
/// The control bar under the picture; floating in fullscreen.
pub const DOCK_H: f32 = 44.;
/// Gap between the floating bar or popups and the screen edge in fullscreen.
pub const FLOAT_GAP: f32 = 16.;
pub const HEADER_H: f32 = 36.;
pub const ROW_H: f32 = 28.;
pub const BUTTON: f32 = 28.;
