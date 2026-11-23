//! Small native window, power, gesture, media-key and thumbnail helpers,
//! isolated by operating system.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub(crate) use macos::{
    MediaKeyBridge, PinchMonitor, SleepPreventer, begin_window_drag, hide_traffic_lights,
    set_always_on_top, titlebar_double_click, toggle_maximize,
};
#[cfg(any(debug_assertions, feature = "devtools"))]
#[cfg(target_os = "macos")]
pub(crate) use macos::{snapshot_window, synth_mouse};
#[cfg(target_os = "windows")]
pub(crate) use windows::{
    MediaKeyBridge, PinchMonitor, SleepPreventer, begin_window_drag, hide_traffic_lights, pipe,
    set_always_on_top, titlebar_double_click, toggle_maximize,
};
#[cfg(any(debug_assertions, feature = "devtools"))]
#[cfg(target_os = "windows")]
pub(crate) use windows::{snapshot_window, synth_mouse};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::RenderImage;
use image::ImageDecoder as _;

pub(crate) const IS_MACOS: bool = cfg!(target_os = "macos");

pub(crate) const REVEAL_IN_FILE_MANAGER_LABEL: &str = if IS_MACOS {
    "在访达中显示"
} else {
    "在资源管理器中显示"
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct PinchEvent {
    pub(crate) delta: f64,
    pub(crate) position: [f64; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MediaKey {
    Play,
    Pause,
    #[cfg(target_os = "macos")]
    Toggle,
    Next,
    Prev,
    #[cfg(target_os = "windows")]
    Seek(f64),
}

pub(crate) struct NowPlaying<'a> {
    pub(crate) title: std::borrow::Cow<'a, str>,
    pub(crate) is_video: bool,
    pub(crate) state: &'a iris_core::player::PlaybackState,
}

/// Default folder for saved screenshots (`Desktop` on macOS, `Pictures`/`Desktop` on Windows).
pub(crate) fn default_screenshot_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            let desktop = PathBuf::from(&h).join("Desktop");
            if desktop.is_dir() {
                return desktop;
            }
            return PathBuf::from(h);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = windows::known_screenshot_dir() {
            return path;
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let pics = PathBuf::from(&home).join("Pictures");
            if pics.is_dir() {
                return pics;
            }
            let desktop = PathBuf::from(&home).join("Desktop");
            if desktop.is_dir() {
                return desktop;
            }
            return PathBuf::from(home);
        }
    }
    std::env::temp_dir()
}

/// Decodes a compact straight-BGRA `RenderImage` thumbnail entirely in memory.
pub(crate) fn decode_thumbnail(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
    let max_dim = max_dim.clamp(1, 2048);
    #[cfg(target_os = "macos")]
    if let Some(img) = macos::decode_thumbnail_imageio(path, max_dim) {
        return Some(img);
    }
    #[cfg(target_os = "windows")]
    if let Some(img) = windows::decode_thumbnail_wic(path, max_dim) {
        return Some(img);
    }
    decode_thumbnail_fallback(path, max_dim)
}

const MAX_SOURCE_DIM: u32 = 16_384;

fn valid_thumbnail_source(width: u64, height: u64) -> bool {
    let limit = u64::from(MAX_SOURCE_DIM);
    (1..=limit).contains(&width) && (1..=limit).contains(&height) && width * height <= 100_000_000
}

/// Bounded cross-platform thumbnail decoder fallback with EXIF orientation support.
fn decode_thumbnail_fallback(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
    const MAX_BYTES: u64 = 128 * 1024 * 1024;
    let mut reader = image::ImageReader::open(path).ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_BYTES);
    limits.max_image_width = Some(MAX_SOURCE_DIM);
    limits.max_image_height = Some(MAX_SOURCE_DIM);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if !valid_thumbnail_source(width.into(), height.into()) {
        return None;
    }
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()?;
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
    let thumb = apply_exif_orientation(
        img.thumbnail(max_dim, max_dim),
        iris_core::photo::exif_orientation(path),
    );
    let mut rgba = thumb.into_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2); // GPUI RenderImage expects straight BGRA.
    }
    Some(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}

pub(crate) fn apply_exif_orientation(
    img: image::DynamicImage,
    orientation: u16,
) -> image::DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}
