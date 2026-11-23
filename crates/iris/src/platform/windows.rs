//! Windows native window, sleep prevention, WIC thumbnail, GDI capture, and
//! same-user named pipe helpers.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedReceiver;
use gpui::RenderImage;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{GENERIC_READ, HWND, LPARAM, WPARAM},
    Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmapSource,
        IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
        WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    },
    System::{
        Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState},
    },
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, VK_LBUTTON},
        WindowsAndMessaging::*,
    },
};
use windows::core::{Interface, PCWSTR};

use super::{MediaKey, PinchEvent, apply_exif_orientation};

fn hwnd(window: &gpui::Window) -> Option<HWND> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

pub(crate) fn hide_traffic_lights(_window: &gpui::Window) {}

pub(crate) fn set_always_on_top(window: &gpui::Window, on: bool) {
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
pub(crate) struct SleepPreventer {
    active: bool,
}

impl SleepPreventer {
    pub(crate) fn new() -> Self {
        Self { active: false }
    }
    pub(crate) fn set_active(&mut self, active: bool) {
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

pub(crate) struct PinchMonitor;
impl PinchMonitor {
    pub(crate) fn attach(_window: &gpui::Window) -> Option<(Self, UnboundedReceiver<PinchEvent>)> {
        None
    }
}

pub(crate) struct MediaKeyBridge;
impl MediaKeyBridge {
    pub(crate) fn attach() -> Option<(Self, UnboundedReceiver<MediaKey>)> {
        None
    }
    pub(crate) fn update_now_playing(
        &self,
        _title: Option<&str>,
        _duration: f64,
        _position: f64,
        _playing: bool,
        _speed: f64,
    ) {
    }
}

/// Fast native WIC thumbnail decoder (supports embedded EXIF thumbnails,
/// hardware-friendly downscaling, and system HEIC/WebP codecs when installed).
pub(super) fn decode_thumbnail_wic(path: &Path, max_dim: u32) -> Option<Arc<RenderImage>> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let com_init = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let res = (|| -> Option<Arc<RenderImage>> {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let decoder = factory
                .CreateDecoderFromFilename(
                    PCWSTR(wide.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
                .ok()?;
            let frame = decoder.GetFrame(0).ok()?;
            let source: IWICBitmapSource = frame.cast().ok()?;
            let (mut sw, mut sh) = (0u32, 0u32);
            source.GetSize(&mut sw, &mut sh).ok()?;
            if sw == 0 || sh == 0 {
                return None;
            }
            let long = sw.max(sh);
            let (dw, dh) = if long > max_dim {
                let ratio = max_dim as f64 / long as f64;
                (
                    (sw as f64 * ratio).round().max(1.) as u32,
                    (sh as f64 * ratio).round().max(1.) as u32,
                )
            } else {
                (sw, sh)
            };
            let scaled: IWICBitmapSource = if (dw, dh) != (sw, sh) {
                let scaler = factory.CreateBitmapScaler().ok()?;
                scaler
                    .Initialize(&source, dw, dh, WICBitmapInterpolationModeFant)
                    .ok()?;
                scaler.cast().ok()?
            } else {
                source
            };
            let conv = factory.CreateFormatConverter().ok()?;
            conv.Initialize(
                &scaled,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
            let stride = dw.checked_mul(4)?;
            let total = (stride as usize).checked_mul(dh as usize)?;
            let mut pixels = vec![0u8; total];
            conv.CopyPixels(std::ptr::null(), stride, &mut pixels)
                .ok()?;
            let orientation = iris_core::photo::exif_orientation(path);
            let buf = image::RgbaImage::from_raw(dw, dh, pixels)?;
            let buf = if orientation != 1 {
                apply_exif_orientation(image::DynamicImage::ImageRgba8(buf), orientation)
                    .into_rgba8()
            } else {
                buf
            };
            Some(Arc::new(RenderImage::new([image::Frame::new(buf)])))
        })();
        if com_init {
            CoUninitialize();
        }
        res
    }
}

#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) fn synth_mouse(
    window: &gpui::Window,
    click: bool,
    x: f64,
    y: f64,
) -> Option<Box<dyn FnOnce()>> {
    let handle = hwnd(window)?.0 as isize;
    let scale = window.scale_factor() as f64;
    let pos =
        LPARAM((((y * scale) as i32 & 0xffff) << 16 | ((x * scale) as i32 & 0xffff)) as isize);
    Some(Box::new(move || unsafe {
        let hwnd = HWND(handle as *mut _);
        let _ = PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), pos);
        if click {
            let _ = PostMessageW(hwnd, WM_LBUTTONDOWN, WPARAM(1), pos);
            let _ = PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), pos);
        }
    }))
}

pub(crate) fn begin_window_drag(window: &gpui::Window) {
    if let Some(hwnd) = hwnd(window) {
        unsafe {
            // Only invoke `SC_DRAGMOVE` (`0xF012`) while the physical left mouse button is
            // held down; otherwise `DefWindowProcW` enters modal keyboard-move mode.
            if (GetAsyncKeyState(i32::from(VK_LBUTTON.0)) as u16 & 0x8000) == 0 {
                return;
            }
            let _ = ReleaseCapture();
            let _ = PostMessageW(hwnd, WM_SYSCOMMAND, WPARAM(0xF012), LPARAM(0));
        }
    }
}

pub(crate) fn toggle_maximize(window: &gpui::Window) {
    if let Some(hwnd) = hwnd(window) {
        unsafe {
            let cmd = if IsZoomed(hwnd).as_bool() || window.is_maximized() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            };
            let _ = ShowWindowAsync(hwnd, cmd);
        }
    }
}

pub(crate) fn titlebar_double_click(window: &gpui::Window) {
    toggle_maximize(window);
}

/// Captures the visible desktop rectangle of this window, including both
/// DComp targets. The window must be unobscured and on-screen; unlike GDI
/// window capture this also sees NOREDIRECTIONBITMAP composition surfaces.
#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) fn snapshot_window(window: &gpui::Window, path: &Path) -> anyhow::Result<()> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::*;
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

/// Same-user named pipes for the debug control channel and the
/// single-instance file handoff.
pub(crate) mod pipe {
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
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
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
    pub(crate) fn current_user_sid() -> io::Result<String> {
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
    pub(crate) fn create_inbound(name: &[u16], in_buffer: u32) -> io::Result<File> {
        debug_assert_eq!(name.last(), Some(&0));
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
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

#[cfg(test)]
mod tests {
    use super::decode_thumbnail_wic;
    use std::path::Path;

    #[test]
    fn wic_thumbnail_decoder_respects_exif_orientations_and_bgra_channels() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/photo");
        let o1 = root.join("exif/exif-o1.jpg");
        if !o1.is_file() {
            return;
        }
        let img_o1 = decode_thumbnail_wic(&o1, 160).expect("WIC decode exif-o1.jpg");
        let sz_o1 = img_o1.size(0);
        assert_eq!(
            (i32::from(sz_o1.width), i32::from(sz_o1.height)),
            (160, 107)
        );

        let img_o3 =
            decode_thumbnail_wic(&root.join("exif/exif-o3.jpg"), 160).expect("WIC decode exif-o3");
        let sz_o3 = img_o3.size(0);
        assert_eq!(
            (i32::from(sz_o3.width), i32::from(sz_o3.height)),
            (160, 107)
        );

        // EXIF orientations 6 (90° CW) and 8 (270° CW) must transpose 3000×2000 -> 107×160.
        for name in ["exif/exif-o6.jpg", "exif/exif-o8.jpg"] {
            let img = decode_thumbnail_wic(&root.join(name), 160).expect(name);
            let sz = img.size(0);
            assert_eq!(
                (i32::from(sz.width), i32::from(sz.height)),
                (107, 160),
                "unexpected oriented size for {name}"
            );
        }

        // Verify straight BGRA channel layout on pure sRGB red PNG (B≈0, G≈0, R≈255, A=255).
        let red_png = root.join("icc/red-srgb.png");
        if red_png.is_file() {
            let red = decode_thumbnail_wic(&red_png, 64).expect("WIC decode red-srgb.png");
            let bytes = red.as_bytes(0).expect("frame 0 bytes");
            assert!(
                bytes[0] < 5 && bytes[1] < 5 && bytes[2] > 250 && bytes[3] == 255,
                "expected BGRA pure red [0,0,255,255], got {:?}",
                &bytes[..4]
            );
        }

        // HEIC: succeeds with oriented dimensions when system HEIF WIC codec is present, or returns None cleanly.
        let heic_o6 = root.join("exif/heic-o6.heic");
        if heic_o6.is_file()
            && let Some(heic_img) = decode_thumbnail_wic(&heic_o6, 160)
        {
            let sz = heic_img.size(0);
            assert!(i32::from(sz.width) > 0 && i32::from(sz.height) > 0);
        }
    }
}
