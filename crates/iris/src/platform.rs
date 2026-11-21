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

#[cfg(target_os = "windows")]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM, RECT},
        Graphics::Gdi::*,
        UI::{Input::KeyboardAndMouse::ReleaseCapture, WindowsAndMessaging::*},
    };

    fn hwnd(window: &gpui::Window) -> Option<HWND> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    pub fn hide_traffic_lights(_window: &gpui::Window) {}

    pub fn synth_mouse(window: &gpui::Window, click: bool, x: f64, y: f64) -> Option<Box<dyn FnOnce()>> {
        let handle = hwnd(window)?.0 as isize;
        let scale = window.scale_factor() as f64;
        let pos = LPARAM((((y * scale) as i32 & 0xffff) << 16 | ((x * scale) as i32 & 0xffff)) as isize);
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
                let _ = PostMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
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
                if !bitmap.0.is_null() { let _ = DeleteObject(bitmap); }
                if !memory.0.is_null() { let _ = DeleteDC(memory); }
                ReleaseDC(None, screen);
                anyhow::bail!("creating screenshot bitmap failed");
            }
            let old = SelectObject(memory, bitmap);
            let copied = BitBlt(memory, 0, 0, w, h, screen, rect.left, rect.top, SRCCOPY | CAPTUREBLT);
            SelectObject(memory, old);
            let mut pixels = vec![0u8; w as usize * h as usize * 4];
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32,
                    biCompression: BI_RGB.0, ..Default::default()
                },
                ..Default::default()
            };
            let rows = if copied.is_ok() {
                GetDIBits(screen, bitmap, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS)
            } else { 0 };
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            copied?;
            anyhow::ensure!(rows == h, "GetDIBits failed");
            for p in pixels.as_chunks_mut::<4>().0 { p.swap(0, 2); p[3] = 255; }
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
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // Conversion APIs allocate with LocalAlloc, not Rust's allocator.
    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0); }
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
            GetTokenInformation(token.as_raw_handle(), TokenUser, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut info = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(token.as_raw_handle(), TokenUser, info.as_mut_ptr().cast(),
                needed, &mut needed) == 0
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
            while *sid.add(length) != 0 { length += 1; }
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
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})").encode_utf16().chain(Some(0)).collect();
        // SAFETY: as above; the descriptor outlives CreateNamedPipeW.
        unsafe {
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(),
                SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _descriptor_owner = LocalAllocation(descriptor);
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let pipe = CreateNamedPipeW(name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1, 0, in_buffer, 0, &security);
            if pipe == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(File::from_raw_handle(pipe))
        }
    }
}
