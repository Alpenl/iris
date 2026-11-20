//! Small native window helpers.

#[cfg(target_os = "macos")]
mod imp {
    use cocoa::base::{YES, id, nil};
    use objc::{class, msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    fn ns_window(window: &gpui::Window) -> Option<id> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return None;
        };
        let view = h.ns_view.as_ptr() as id;
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
        use cocoa::foundation::{NSPoint, NSRect};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return None;
        };
        let view = h.ns_view.as_ptr() as id;
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

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn hide_traffic_lights(_window: &gpui::Window) {}
    pub fn synth_mouse(
        _window: &gpui::Window,
        _click: bool,
        _x: f64,
        _y: f64,
    ) -> Option<Box<dyn FnOnce()>> {
        eprintln!("[iris/control] synthetic mouse input is macOS-only");
        None
    }
    pub fn begin_window_drag(window: &gpui::Window) {
        window.start_window_move();
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

#[cfg(not(target_os = "macos"))]
pub fn snapshot_window(_window: &gpui::Window, _path: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("snapshot not implemented on this platform")
}
