//! macOS native window, sleep prevention, trackpad pinch, media key,
//! ImageIO thumbnail, and debug capture helpers.

use std::ffi::{CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Once};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::RenderImage;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{MediaKey, PinchEvent};

fn ns_view(window: &gpui::Window) -> Option<id> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    Some(h.ns_view.as_ptr() as id)
}

fn ns_window(window: &gpui::Window) -> Option<id> {
    let view = ns_view(window)?;
    let win: id = unsafe { msg_send![view, window] };
    (win != nil).then_some(win)
}

/// Hides the system close/minimise/zoom buttons; Iris draws its own.
pub fn hide_traffic_lights(window: &gpui::Window) {
    let Some(win) = ns_window(window) else { return };
    for kind in 0u64..3 {
        unsafe {
            let button: id = msg_send![win, standardWindowButton: kind];
            if button != nil {
                let _: () = msg_send![button, setHidden: YES];
            }
        }
    }
}

/// Floats the window above normal windows (`NSFloatingWindowLevel = 3`).
pub fn set_always_on_top(window: &gpui::Window, on: bool) {
    let Some(win) = ns_window(window) else { return };
    let level: isize = if on { 3 } else { 0 };
    unsafe {
        let _: () = msg_send![win, setLevel: level];
    }
}

// ---- idle display sleep prevention (IOKit) ----

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,
        assertion_level: u32,
        assertion_name: *const c_void,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        s: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: *const c_void,
        buf: *const u8,
        len: isize,
        is_dir: bool,
    ) -> *const c_void;
    fn CFNumberCreate(
        alloc: *const c_void,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> *const c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_cb: *const c_void,
        val_cb: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[derive(Default)]
pub struct SleepPreventer {
    assertion_id: Option<u32>,
}

impl SleepPreventer {
    pub fn new() -> Self {
        Self { assertion_id: None }
    }

    pub fn set_active(&mut self, active: bool) {
        if active && self.assertion_id.is_none() {
            let ty = CString::new("NoDisplaySleepAssertion").unwrap();
            let reason = CString::new("Iris video playback").unwrap();
            unsafe {
                let cf_ty = CFStringCreateWithCString(ptr::null(), ty.as_ptr(), 0x0800_0100);
                let cf_reason =
                    CFStringCreateWithCString(ptr::null(), reason.as_ptr(), 0x0800_0100);
                let mut id_out: u32 = 0;
                // kIOPMAssertionLevelOn = 255.
                if IOPMAssertionCreateWithName(cf_ty, 255, cf_reason, &mut id_out) == 0 {
                    self.assertion_id = Some(id_out);
                }
                CFRelease(cf_reason);
                CFRelease(cf_ty);
            }
        } else if !active && let Some(id_val) = self.assertion_id.take() {
            unsafe {
                let _ = IOPMAssertionRelease(id_val);
            }
        }
    }
}

impl Drop for SleepPreventer {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

// ---- native trackpad pinch (NSMagnificationGestureRecognizer) ----

struct PinchTargetState {
    tx: UnboundedSender<PinchEvent>,
    view: id,
}

extern "C" fn on_magnify(this: &Object, _: Sel, recognizer: id) {
    unsafe {
        let p: *mut c_void = *this.get_ivar("irisPinch");
        if p.is_null() {
            return;
        }
        let state = &*(p as *const PinchTargetState);
        let mag: f64 = msg_send![recognizer, magnification];
        let _: () = msg_send![recognizer, setMagnification: 0.0f64];
        if !mag.is_finite() || mag.abs() < 1e-4 {
            return;
        }
        let loc: NSPoint = msg_send![recognizer, locationInView: state.view];
        let bounds: NSRect = msg_send![state.view, bounds];
        let flipped: cocoa::base::BOOL = msg_send![state.view, isFlipped];
        let y = if flipped == YES {
            loc.y
        } else {
            bounds.size.height - loc.y
        };
        let _ = state.tx.unbounded_send(PinchEvent {
            delta: mag * 1.6,
            position: [loc.x, y],
        });
    }
}

fn pinch_target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl =
            ClassDecl::new("IrisPinchTarget", class!(NSObject)).expect("IrisPinchTarget class");
        decl.add_ivar::<*mut c_void>("irisPinch");
        unsafe {
            decl.add_method(
                sel!(onMagnify:),
                on_magnify as extern "C" fn(&Object, Sel, id),
            );
        }
        decl.register();
    });
    Class::get("IrisPinchTarget").expect("IrisPinchTarget")
}

pub struct PinchMonitor {
    view: id,
    recognizer: id,
    target: id,
    state: *mut PinchTargetState,
}

impl PinchMonitor {
    pub fn attach(window: &gpui::Window) -> Option<(Self, UnboundedReceiver<PinchEvent>)> {
        let view = ns_view(window)?;
        let (tx, rx) = unbounded();
        let state = Box::into_raw(Box::new(PinchTargetState { tx, view }));
        unsafe {
            // Retain the NSView so Drop never messages a deallocated view during window close.
            let _: id = msg_send![view, retain];
            let target: id = msg_send![pinch_target_class(), new];
            (*target).set_ivar("irisPinch", state as *mut c_void);
            let rec: id = msg_send![class!(NSMagnificationGestureRecognizer), alloc];
            let rec: id = msg_send![rec, initWithTarget: target action: sel!(onMagnify:)];
            let _: () = msg_send![view, addGestureRecognizer: rec];
            Some((
                Self {
                    view,
                    recognizer: rec,
                    target,
                    state,
                },
                rx,
            ))
        }
    }
}

impl Drop for PinchMonitor {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.view, removeGestureRecognizer: self.recognizer];
            (*self.target).set_ivar("irisPinch", ptr::null_mut::<c_void>());
            let _: () = msg_send![self.recognizer, release];
            let _: () = msg_send![self.target, release];
            let _: () = msg_send![self.view, release];
            drop(Box::from_raw(self.state));
        }
    }
}

// ---- macOS system media keys & Now Playing (MediaPlayer.framework) ----

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

struct MediaTargetState {
    tx: UnboundedSender<MediaKey>,
}

fn send_media_key(this: &Object, key: MediaKey) -> isize {
    unsafe {
        let p: *mut c_void = *this.get_ivar("irisMedia");
        if !p.is_null() {
            let state = &*(p as *const MediaTargetState);
            let _ = state.tx.unbounded_send(key);
        }
    }
    0 // MPRemoteCommandHandlerStatusSuccess
}

extern "C" fn cmd_play(this: &Object, _: Sel, _ev: id) -> isize {
    send_media_key(this, MediaKey::Play)
}
extern "C" fn cmd_pause(this: &Object, _: Sel, _ev: id) -> isize {
    send_media_key(this, MediaKey::Pause)
}
extern "C" fn cmd_toggle(this: &Object, _: Sel, _ev: id) -> isize {
    send_media_key(this, MediaKey::Toggle)
}
extern "C" fn cmd_next(this: &Object, _: Sel, _ev: id) -> isize {
    send_media_key(this, MediaKey::Next)
}
extern "C" fn cmd_prev(this: &Object, _: Sel, _ev: id) -> isize {
    send_media_key(this, MediaKey::Prev)
}

fn media_target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl = ClassDecl::new("IrisMediaKeyTarget", class!(NSObject))
            .expect("IrisMediaKeyTarget class");
        decl.add_ivar::<*mut c_void>("irisMedia");
        unsafe {
            decl.add_method(
                sel!(onPlay:),
                cmd_play as extern "C" fn(&Object, Sel, id) -> isize,
            );
            decl.add_method(
                sel!(onPause:),
                cmd_pause as extern "C" fn(&Object, Sel, id) -> isize,
            );
            decl.add_method(
                sel!(onToggle:),
                cmd_toggle as extern "C" fn(&Object, Sel, id) -> isize,
            );
            decl.add_method(
                sel!(onNext:),
                cmd_next as extern "C" fn(&Object, Sel, id) -> isize,
            );
            decl.add_method(
                sel!(onPrev:),
                cmd_prev as extern "C" fn(&Object, Sel, id) -> isize,
            );
        }
        decl.register();
    });
    Class::get("IrisMediaKeyTarget").expect("IrisMediaKeyTarget")
}

pub struct MediaKeyBridge {
    target: id,
    state: *mut MediaTargetState,
}

impl MediaKeyBridge {
    pub fn attach() -> Option<(Self, UnboundedReceiver<MediaKey>)> {
        let path =
            CString::new("/System/Library/Frameworks/MediaPlayer.framework/MediaPlayer").unwrap();
        unsafe {
            if dlopen(path.as_ptr(), 1).is_null() {
                return None;
            }
            let center_cls = Class::get("MPRemoteCommandCenter")?;
            let center: id = msg_send![center_cls, sharedCommandCenter];
            if center == nil {
                return None;
            }
            let (tx, rx) = unbounded();
            let state = Box::into_raw(Box::new(MediaTargetState { tx }));
            let target: id = msg_send![media_target_class(), new];
            (*target).set_ivar("irisMedia", state as *mut c_void);

            let wire = |cmd_sel: Sel, action: Sel| {
                let cmd: id = msg_send![center, performSelector: cmd_sel];
                if cmd != nil {
                    let _: () = msg_send![cmd, setEnabled: YES];
                    let _: () = msg_send![cmd, addTarget: target action: action];
                }
            };
            wire(sel!(playCommand), sel!(onPlay:));
            wire(sel!(pauseCommand), sel!(onPause:));
            wire(sel!(togglePlayPauseCommand), sel!(onToggle:));
            wire(sel!(nextTrackCommand), sel!(onNext:));
            wire(sel!(previousTrackCommand), sel!(onPrev:));

            Some((Self { target, state }, rx))
        }
    }

    pub fn update_now_playing(
        &self,
        title: Option<&str>,
        duration: f64,
        position: f64,
        playing: bool,
        speed: f64,
    ) {
        unsafe {
            let Some(info_cls) = Class::get("MPNowPlayingInfoCenter") else {
                return;
            };
            let center: id = msg_send![info_cls, defaultCenter];
            if center == nil {
                return;
            }
            let Some(title) = title else {
                let _: () = msg_send![center, setNowPlayingInfo: nil];
                let _: () = msg_send![center, setPlaybackState: 0usize];
                return;
            };
            let dict: id = msg_send![class!(NSMutableDictionary), dictionary];
            let ns_str = |s: &str| -> id {
                let c = CString::new(s).unwrap_or_default();
                msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
            };
            let ns_num = |v: f64| -> id { msg_send![class!(NSNumber), numberWithDouble: v] };
            let _: () = msg_send![dict, setObject: ns_str(title) forKey: ns_str("title")];
            if duration > 0. {
                let _: () =
                    msg_send![dict, setObject: ns_num(duration) forKey: ns_str("playbackDuration")];
                let _: () = msg_send![dict, setObject: ns_num(position.max(0.)) forKey: ns_str("MPNowPlayingInfoPropertyElapsedPlaybackTime")];
            }
            let rate = if playing { speed.clamp(0.1, 8.0) } else { 0.0 };
            let _: () = msg_send![dict, setObject: ns_num(rate) forKey: ns_str("MPNowPlayingInfoPropertyPlaybackRate")];
            let _: () = msg_send![center, setNowPlayingInfo: dict];
            // MPNowPlayingPlaybackStatePlaying = 1, Paused = 2.
            let pb_state: usize = if playing { 1 } else { 2 };
            let _: () = msg_send![center, setPlaybackState: pb_state];
        }
    }
}

impl Drop for MediaKeyBridge {
    fn drop(&mut self) {
        unsafe {
            if let Some(center_cls) = Class::get("MPRemoteCommandCenter") {
                let center: id = msg_send![center_cls, sharedCommandCenter];
                if center != nil {
                    for cmd_sel in [
                        sel!(playCommand),
                        sel!(pauseCommand),
                        sel!(togglePlayPauseCommand),
                        sel!(nextTrackCommand),
                        sel!(previousTrackCommand),
                    ] {
                        let cmd: id = msg_send![center, performSelector: cmd_sel];
                        if cmd != nil {
                            let _: () = msg_send![cmd, removeTarget: self.target];
                            let _: () = msg_send![cmd, setEnabled: NO];
                        }
                    }
                }
            }
            if let Some(info_cls) = Class::get("MPNowPlayingInfoCenter") {
                let center: id = msg_send![info_cls, defaultCenter];
                if center != nil {
                    let _: () = msg_send![center, setNowPlayingInfo: nil];
                    let _: () = msg_send![center, setPlaybackState: 0usize];
                }
            }
            (*self.target).set_ivar("irisMedia", ptr::null_mut::<c_void>());
            let _: () = msg_send![self.target, release];
            drop(Box::from_raw(self.state));
        }
    }
}

// ---- fast macOS ImageIO thumbnail decoder (supports HEIC/AVIF/JPEG/PNG/WebP/TIFF) ----

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
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,
        ty: *const c_void,
        count: usize,
        opts: *const c_void,
    ) -> *mut c_void;
    fn CGImageDestinationAddImage(dest: *mut c_void, image: *mut c_void, props: *const c_void);
    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
    static kCGImageSourceCreateThumbnailFromImageAlways: *const c_void;
    static kCGImageSourceCreateThumbnailWithTransform: *const c_void;
    static kCGImageSourceThumbnailMaxPixelSize: *const c_void;
}

pub(super) fn decode_thumbnail_imageio(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
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
        // Convert P3/wide-gamut thumbnails accurately into sRGB for GPUI RenderImage.
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

/// Debug tooling: returns a sender that feeds GPUI's view a synthetic
/// mouse move (and, with `click`, a left click) at (x, y) in GPUI's
/// top-left window points. Call the sender outside any GPUI update: GPUI
/// drops input that arrives while the window is being updated.
pub fn synth_mouse(
    window: &gpui::Window,
    click: bool,
    x: f64,
    y: f64,
) -> Option<Box<dyn FnOnce()>> {
    let view = ns_view(window)?;
    let win = ns_window(window)?;
    Some(Box::new(move || unsafe {
        let content: id = msg_send![win, contentView];
        let frame: NSRect = msg_send![content, frame];
        let loc = NSPoint::new(x, frame.size.height - y);
        let number: isize = msg_send![win, windowNumber];
        let make = |ty: u64, clicks: isize| -> id {
            msg_send![class!(NSEvent), mouseEventWithType: ty location: loc modifierFlags: 0u64
                timestamp: 0.0f64 windowNumber: number context: nil eventNumber: 0isize
                clickCount: clicks pressure: 1.0f32]
        };
        // NSEventType: LeftMouseDown 1, LeftMouseUp 2, MouseMoved 5.
        let _: () = msg_send![view, mouseMoved: make(5, 0)];
        if click {
            let _: () = msg_send![view, mouseDown: make(1, 1)];
            let _: () = msg_send![view, mouseUp: make(2, 1)];
        }
    }))
}

/// Starts a native window drag from the current mouse-down event.
pub fn begin_window_drag(window: &gpui::Window) {
    let Some(win) = ns_window(window) else { return };
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let event: id = msg_send![app, currentEvent];
        if event != nil {
            let _: () = msg_send![win, performWindowDragWithEvent: event];
        }
    }
}

/// Captures this app's own window to a PNG (debug tooling; own windows need
/// no screen-recording permission).
pub fn snapshot_window(window: &gpui::Window, path: &Path) -> anyhow::Result<()> {
    let view = ns_view(window).ok_or_else(|| anyhow::anyhow!("not an AppKit window"))?;
    unsafe {
        let win: id = msg_send![view, window];
        let number: isize = msg_send![win, windowNumber];
        let null_rect = CGRect {
            x: f64::INFINITY,
            y: f64::INFINITY,
            w: 0.,
            h: 0.,
        };
        // IncludingWindow = 1<<3; BoundsIgnoreFraming = 1<<0 | BestResolution = 1<<3.
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
        let ty_c = CString::new("public.png").unwrap();
        let ty = CFStringCreateWithCString(ptr::null(), ty_c.as_ptr(), 0x0800_0100);
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
