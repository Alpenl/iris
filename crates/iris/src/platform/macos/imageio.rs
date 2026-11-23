//! Fast macOS ImageIO thumbnail decoder (supports HEIC/AVIF/JPEG/PNG/WebP/TIFF)
//! and own-window debug PNG snapshot helper.

use std::ffi::c_void;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use gpui::RenderImage;

use super::{
    CFDictionaryCreate, CFNumberCreate, CFRelease, CFURLCreateFromFileSystemRepresentation,
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: *const c_void;
    fn CGImageGetWidth(image: *mut c_void) -> usize;
    fn CGImageGetHeight(image: *mut c_void) -> usize;
    fn CGImageRelease(image: *mut c_void);
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut c_void;
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *mut c_void);
    fn CGContextRelease(ctx: *mut c_void);
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn CGWindowListCreateImage(
        rect: CGRect,
        option: u32,
        window_id: u32,
        image_option: u32,
    ) -> *mut c_void;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithURL(url: *const c_void, options: *const c_void) -> *mut c_void;
    fn CGImageSourceCreateThumbnailAtIndex(
        isrc: *mut c_void,
        index: usize,
        options: *const c_void,
    ) -> *mut c_void;
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,
        ty: *const c_void,
        count: usize,
        opts: *const c_void,
    ) -> *mut c_void;
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn CGImageDestinationAddImage(dest: *mut c_void, image: *mut c_void, props: *const c_void);
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
    static kCGImageSourceCreateThumbnailFromImageAlways: *const c_void;
    static kCGImageSourceCreateThumbnailWithTransform: *const c_void;
    static kCGImageSourceThumbnailMaxPixelSize: *const c_void;
}

pub(in crate::platform) fn decode_thumbnail_imageio(
    path: &Path,
    max_dim: u32,
) -> Option<Arc<RenderImage>> {
    let bytes = path.as_os_str().as_bytes();
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            false,
        );
        if url.is_null() {
            return None;
        }
        let src = CGImageSourceCreateWithURL(url, ptr::null());
        CFRelease(url);
        if src.is_null() {
            return None;
        }
        let max_i32 = max_dim as i32;
        // kCFNumberSInt32Type = 3.
        let num = CFNumberCreate(ptr::null(), 3, &max_i32 as *const i32 as *const c_void);
        let keys = [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceThumbnailMaxPixelSize,
        ];
        let vals = [kCFBooleanTrue, kCFBooleanTrue, num];
        let opts = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            3,
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        );
        let cg_img = CGImageSourceCreateThumbnailAtIndex(src, 0, opts);
        CFRelease(opts);
        CFRelease(num);
        CFRelease(src);
        if cg_img.is_null() {
            return None;
        }
        let w = CGImageGetWidth(cg_img);
        let h = CGImageGetHeight(cg_img);
        if w == 0 || h == 0 || w > 2048 || h > 2048 {
            CGImageRelease(cg_img);
            return None;
        }
        let mut pixels = vec![0u8; w * h * 4];
        let mut cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        if cs.is_null() {
            cs = CGColorSpaceCreateDeviceRGB();
        }
        // kCGBitmapByteOrder32Little (2<<12) | kCGImageAlphaPremultipliedFirst (2) => BGRA8 premul.
        let ctx = CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            cs,
            (2 << 12) | 2,
        );
        let drawn = !ctx.is_null();
        if drawn {
            CGContextDrawImage(
                ctx,
                CGRect {
                    x: 0.,
                    y: 0.,
                    w: w as f64,
                    h: h as f64,
                },
                cg_img,
            );
            CGContextRelease(ctx);
        }
        CGColorSpaceRelease(cs);
        CGImageRelease(cg_img);
        if !drawn {
            return None;
        }
        // GPUI RenderImage expects straight (unpremultiplied) BGRA.
        for px in pixels.as_chunks_mut::<4>().0 {
            let a = px[3];
            if a > 0 && a < 255 {
                let scale = 255.0 / a as f32;
                px[0] = (px[0] as f32 * scale).min(255.0) as u8;
                px[1] = (px[1] as f32 * scale).min(255.0) as u8;
                px[2] = (px[2] as f32 * scale).min(255.0) as u8;
            }
        }
        let buf = image::RgbaImage::from_raw(w as u32, h as u32, pixels)?;
        Some(Arc::new(RenderImage::new([image::Frame::new(buf)])))
    }
}

/// Captures this app's own window to a PNG (debug tooling; own windows need
/// no screen-recording permission).
#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) fn snapshot_window(window: &gpui::Window, path: &Path) -> anyhow::Result<()> {
    use cocoa::base::id;
    use objc::{msg_send, sel, sel_impl};

    let view = super::ns_view(window).ok_or_else(|| anyhow::anyhow!("not an AppKit window"))?;
    unsafe {
        let win: id = msg_send![view, window];
        let number: isize = msg_send![win, windowNumber];
        let null_rect = CGRect {
            x: f64::INFINITY,
            y: f64::INFINITY,
            w: 0.,
            h: 0.,
        };
        let image = CGWindowListCreateImage(null_rect, 1 << 3, number as u32, (1 << 0) | (1 << 3));
        if image.is_null() {
            anyhow::bail!("CGWindowListCreateImage returned null");
        }
        let bytes = path.as_os_str().as_bytes();
        let url = CFURLCreateFromFileSystemRepresentation(
            ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            false,
        );
        let ty = super::CFStringCreateWithCString(ptr::null(), c"public.png".as_ptr(), 0x0800_0100);
        let dest = CGImageDestinationCreateWithURL(url, ty, 1, ptr::null());
        let ok = !dest.is_null() && {
            CGImageDestinationAddImage(dest, image, ptr::null());
            CGImageDestinationFinalize(dest)
        };
        if !dest.is_null() {
            CFRelease(dest);
        }
        CFRelease(ty);
        CFRelease(url);
        CGImageRelease(image);
        if !ok {
            anyhow::bail!("writing PNG failed");
        }
    }
    Ok(())
}
