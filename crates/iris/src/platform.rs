//! Small native window, power, gesture, media-key and thumbnail helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::RenderImage;

#[derive(Clone, Copy, Debug)]
pub struct PinchEvent {
    pub delta: f64,
    pub position: [f64; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKey {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
}

/// Default folder for saved screenshots (`Desktop` on macOS, `Pictures`/`Desktop` on Windows).
pub fn default_screenshot_dir() -> PathBuf {
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

/// Decodes a compact BGRA `RenderImage` thumbnail entirely in memory.
pub fn decode_thumbnail(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
    #[cfg(target_os = "macos")]
    if let Some(img) = imp::decode_thumbnail_imageio(path, max_dim) {
        return Some(img);
    }
    let img = image::open(path).ok()?;
    let thumb = img.thumbnail(max_dim, max_dim);
    let mut rgba = thumb.into_rgba8();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // GPUI RenderImage expects BGRA.
    }
    Some(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_char, c_void};
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
            } else if !active
                && let Some(id_val) = self.assertion_id.take()
            {
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
            let mut decl = ClassDecl::new("IrisPinchTarget", class!(NSObject))
                .expect("IrisPinchTarget class");
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
                CString::new("/System/Library/Frameworks/MediaPlayer.framework/MediaPlayer")
                    .unwrap();
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
                    let _: () = msg_send![dict, setObject: ns_num(duration) forKey: ns_str("playbackDuration")];
                    let _: () = msg_send![dict, setObject: ns_num(position.max(0.)) forKey: ns_str("MPNowPlayingInfoPropertyElapsedPlaybackTime")];
                }
                let rate = if playing { 1.0 } else { 0.0 };
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
        fn CGImageGetWidth(image: *mut c_void) -> usize;
        fn CGImageGetHeight(image: *mut c_void) -> usize;
        fn CGImageRelease(image: *mut c_void);
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
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithURL(url: *const c_void, options: *const c_void) -> *mut c_void;
        fn CGImageSourceCreateThumbnailAtIndex(
            isrc: *mut c_void,
            index: usize,
            options: *const c_void,
        ) -> *mut c_void;
        static kCGImageSourceCreateThumbnailFromImageAlways: *const c_void;
        static kCGImageSourceCreateThumbnailWithTransform: *const c_void;
        static kCGImageSourceThumbnailMaxPixelSize: *const c_void;
    }

    pub fn decode_thumbnail_imageio(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
        let p = path.to_string_lossy();
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(
                ptr::null(),
                p.as_ptr(),
                p.len() as isize,
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
            let cs = CGColorSpaceCreateDeviceRGB();
            // kCGBitmapByteOrder32Little (2<<12) | kCGImageAlphaPremultipliedFirst (2) => BGRA8.
            let ctx = CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                cs,
                (2 << 12) | 2,
            );
            if !ctx.is_null() {
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
}

#[cfg(target_os = "windows")]
mod imp {
    use futures::channel::mpsc::UnboundedReceiver;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::*,
        System::Power::{
            ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
        },
        UI::{Input::KeyboardAndMouse::ReleaseCapture, WindowsAndMessaging::*},
    };

    use super::{MediaKey, PinchEvent};

    fn hwnd(window: &gpui::Window) -> Option<HWND> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    pub fn hide_traffic_lights(_window: &gpui::Window) {}

    pub fn set_always_on_top(window: &gpui::Window, on: bool) {
        if let Some(hwnd) = hwnd(window) {
            let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    after,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    #[derive(Default)]
    pub struct SleepPreventer {
        active: bool,
    }

    impl SleepPreventer {
        pub fn new() -> Self {
            Self { active: false }
        }
        pub fn set_active(&mut self, active: bool) {
            if active == self.active {
                return;
            }
            self.active = active;
            unsafe {
                let flags = if active {
                    ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
                } else {
                    ES_CONTINUOUS
                };
                let _ = SetThreadExecutionState(flags);
            }
        }
    }

    impl Drop for SleepPreventer {
        fn drop(&mut self) {
            self.set_active(false);
        }
    }

    pub struct PinchMonitor;
    impl PinchMonitor {
        pub fn attach(_window: &gpui::Window) -> Option<(Self, UnboundedReceiver<PinchEvent>)> {
            None
        }
    }

    pub struct MediaKeyBridge;
    impl MediaKeyBridge {
        pub fn attach() -> Option<(Self, UnboundedReceiver<MediaKey>)> {
            None
        }
        pub fn update_now_playing(
            &self,
            _title: Option<&str>,
            _duration: f64,
            _position: f64,
            _playing: bool,
        ) {
        }
    }

    pub fn synth_mouse(
        window: &gpui::Window,
        click: bool,
        x: f64,
        y: f64,
    ) -> Option<Box<dyn FnOnce()>> {
        let handle = hwnd(window)?.0 as isize;
        let scale = window.scale_factor() as f64;
        let pos = LPARAM(
            (((y * scale) as i32 & 0xffff) << 16 | ((x * scale) as i32 & 0xffff)) as isize,
        );
        Some(Box::new(move || unsafe {
            let hwnd = HWND(handle as *mut _);
            let _ = PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), pos);
            if click {
                let _ = PostMessageW(hwnd, WM_LBUTTONDOWN, WPARAM(1), pos);
                let _ = PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), pos);
            }
        }))
    }

    pub fn begin_window_drag(window: &gpui::Window) {
        if let Some(hwnd) = hwnd(window) {
            unsafe {
                let _ = ReleaseCapture();
                let _ = PostMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    WPARAM(HTCAPTION as usize),
                    LPARAM(0),
                );
            }
        }
    }

    /// Captures the visible desktop rectangle of this window, including both
    /// DComp targets. The window must be unobscured and on-screen; unlike GDI
    /// window capture this also sees NOREDIRECTIONBITMAP composition surfaces.
    pub fn snapshot_window(window: &gpui::Window, path: &std::path::Path) -> anyhow::Result<()> {
        let hwnd = hwnd(window).ok_or_else(|| anyhow::anyhow!("missing HWND"))?;
        unsafe {
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect)?;
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            anyhow::ensure!(w > 0 && h > 0, "window is not visible");
            let screen = GetDC(None);
            anyhow::ensure!(!screen.0.is_null(), "GetDC failed");
            let memory = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            if memory.0.is_null() || bitmap.0.is_null() {
                if !bitmap.0.is_null() {
                    let _ = DeleteObject(bitmap);
                }
                if !memory.0.is_null() {
                    let _ = DeleteDC(memory);
                }
                ReleaseDC(None, screen);
                anyhow::bail!("creating screenshot bitmap failed");
            }
            let old = SelectObject(memory, bitmap);
            let copied = BitBlt(
                memory,
                0,
                0,
                w,
                h,
                screen,
                rect.left,
                rect.top,
                SRCCOPY | CAPTUREBLT,
            );
            SelectObject(memory, old);
            let mut pixels = vec![0u8; w as usize * h as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = if copied.is_ok() {
                GetDIBits(
                    screen,
                    bitmap,
                    0,
                    h as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } else {
                0
            };
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            copied?;
            anyhow::ensure!(rows == h, "GetDIBits failed");
            for p in pixels.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
                p[3] = 255;
            }
            image::save_buffer(path, &pixels, w as u32, h as u32, image::ColorType::Rgba8)?;
        }
        Ok(())
    }
}

pub use imp::*;

/// Captures this app's own window to a PNG (debug tooling; own windows need
/// no screen-recording permission).
#[cfg(target_os = "macos")]
pub fn snapshot_window(window: &gpui::Window, path: &std::path::Path) -> anyhow::Result<()> {
    use cocoa::base::id;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::{CString, c_void};

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
        fn CGWindowListCreateImage(
            rect: CGRect,
            option: u32,
            window_id: u32,
            image_option: u32,
        ) -> *mut c_void;
        fn CGImageRelease(image: *mut c_void);
    }
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageDestinationCreateWithURL(
            url: *const c_void,
            ty: *const c_void,
            count: usize,
            opts: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(dest: *mut c_void, image: *mut c_void, props: *const c_void);
        fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            alloc: *const c_void,
            buf: *const u8,
            len: isize,
            is_dir: bool,
        ) -> *const c_void;
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const std::ffi::c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    let handle = HasWindowHandle::window_handle(window).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        anyhow::bail!("not an AppKit window")
    };
    let view = h.ns_view.as_ptr() as id;
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
        let p = path.to_string_lossy();
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            p.as_ptr(),
            p.len() as isize,
            false,
        );
        let ty_c = CString::new("public.png").unwrap();
        let ty = CFStringCreateWithCString(std::ptr::null(), ty_c.as_ptr(), 0x0800_0100);
        let dest = CGImageDestinationCreateWithURL(url, ty, 1, std::ptr::null());
        let ok = !dest.is_null() && {
            CGImageDestinationAddImage(dest, image, std::ptr::null());
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

#[cfg(target_os = "windows")]
pub use imp::snapshot_window;

/// Same-user named pipes for the debug control channel and the
/// single-instance file handoff.
#[cfg(target_os = "windows")]
pub mod pipe {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // Conversion APIs allocate with LocalAlloc, not Rust's allocator.
    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    /// The current process token's user SID in string form (`S-1-5-...`).
    pub fn current_user_sid() -> io::Result<String> {
        // SAFETY: all Win32 buffers are owned, aligned, and remain live through
        // their calls; successful OS handles/allocations immediately gain owners.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle::from_raw_handle(token);
            let mut needed = 0;
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut info = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                info.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let user = &*info.as_ptr().cast::<TOKEN_USER>();
            let mut sid = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let _sid_owner = LocalAllocation(sid.cast());
            let mut length = 0;
            while *sid.add(length) != 0 {
                length += 1;
            }
            String::from_utf16(std::slice::from_raw_parts(sid, length))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
    }

    /// Creates the first and only instance of an inbound, local-only byte pipe
    /// whose protected DACL grants access to the current token's user alone.
    /// `name` is NUL-terminated UTF-16.
    pub fn create_inbound(name: &[u16], in_buffer: u32) -> io::Result<File> {
        debug_assert_eq!(name.last(), Some(&0));
        let sid = current_user_sid()?;
        // Protected DACL: no inherited grants, only the current token's user.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: as above; the descriptor outlives CreateNamedPipeW.
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _descriptor_owner = LocalAllocation(descriptor);
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let pipe = CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                in_buffer,
                0,
                &security,
            );
            if pipe == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(File::from_raw_handle(pipe))
        }
    }
}
