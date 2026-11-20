//! Direction C "Modular panels" tokens.

use gpui::{Hsla, Rgba, rgb, rgba};

pub const FONT_UI: &str = "Space Grotesk";
pub const FONT_MONO: &str = "IBM Plex Mono";

pub fn window_bg() -> Rgba {
    rgb(0x0E0F11)
}
pub fn panel() -> Rgba {
    rgb(0x15171A)
}
pub fn control() -> Rgba {
    rgb(0x1B1E22)
}
pub fn line() -> Rgba {
    rgb(0x2A2E33)
}
pub fn text() -> Rgba {
    rgb(0xE6E8EB)
}
pub fn text_dim() -> Rgba {
    rgb(0x8D939A)
}
pub fn accent() -> Rgba {
    rgb(0xFF5B1F)
}
/// Panel colour at 88% for overlays drawn on top of the video.
pub fn overlay() -> Rgba {
    rgba(0x15171AE0)
}
pub fn accent_soft() -> Hsla {
    let mut c: Hsla = accent().into();
    c.a = 0.14;
    c
}

pub const TITLEBAR_H: f32 = 36.;
pub const DIRECTORY_W: f32 = 288.;
pub const RAIL_W: f32 = 48.;
pub const DOCK_H: f32 = 120.;
