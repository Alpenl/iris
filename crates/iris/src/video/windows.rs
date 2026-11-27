//! Independent D3D11 device, composed below GPUI on the same HWND.
//! Only the render worker touches its immediate context and mpv render API.

use std::cell::Cell;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::VideoReset;
use anyhow::{Context, Result, anyhow};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_core::logging::{self, EventLevel};
use iris_mpv::render::{ActiveVideoTrack, RenderContext, RenderHandle};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HMODULE, HWND, RECT},
    Graphics::{
        Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
            ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
        },
        DirectComposition::{
            DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
        },
        Dxgi::{
            Common::*, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
            DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
            IDXGIDevice, IDXGIFactory2, IDXGISwapChain1,
        },
    },
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
    UI::WindowsAndMessaging::{GetClientRect, IsIconic},
};
use windows::core::Interface;

const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(500);
const VID_CAPTURE_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_TEXTURE_DIM: f64 = 16_384.;

#[derive(Clone, Copy, PartialEq)]
struct Layout {
    rect: [f64; 4],
    scale: f64,
    monitor_check: u64,
    #[cfg(any(debug_assertions, feature = "devtools"))]
    force_reset: bool,
    stopped: bool,
}

pub(crate) struct VideoSurface {
    layout: Arc<Mutex<Layout>>,
    wake: mpsc::SyncSender<()>,
    worker: Option<JoinHandle<()>>,
    scale: Cell<f64>,
    last_monitor_check: Instant,
}

impl VideoSurface {
    pub(crate) fn attach(
        window: &gpui::Window,
        mpv: RenderHandle,
    ) -> Result<(Self, UnboundedReceiver<VideoReset>)> {
        let hwnd = match HasWindowHandle::window_handle(window)
            .map_err(|e| anyhow!("{e:?}"))?
            .as_raw()
        {
            RawWindowHandle::Win32(h) => h.hwnd.get(),
            h => return Err(anyhow!("unexpected window handle {h:?}")),
        };
        // SDR output; use the monitor's ICC when one is installed. HDR/EDR is
        // intentionally not inferred from a 10-bit source or an HDR desktop.
        mpv.set_display_color_target("bt.709", "srgb")?;
        let initial_monitor = monitor_of(HWND(hwnd as *mut _));
        update_display_profile(initial_monitor, &mpv);
        let render_mpv = mpv.clone();
        let raw_scale = window.scale_factor() as f64;
        let scale = if raw_scale.is_finite() && raw_scale > 0. {
            raw_scale
        } else {
            1.
        };
        let layout = Arc::new(Mutex::new(Layout {
            rect: [0., 0., 1., 1.],
            scale,
            monitor_check: 0,
            #[cfg(any(debug_assertions, feature = "devtools"))]
            force_reset: false,
            stopped: false,
        }));
        let (wake, rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (reset_tx, reset_rx) = unbounded();
        let shared = layout.clone();
        let callback_wake = wake.clone();
        let worker = iris_core::concurrency::spawn_thread(
            iris_core::concurrency::ThreadRole::VideoSurface,
            move || {
                // COM and every D3D/render resource remain on this thread.
                let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                let raw_hwnd = HWND(hwnd as *mut _);
                let renderer =
                    unsafe { Renderer::new(raw_hwnd, render_mpv.clone(), callback_wake.clone()) };
                match renderer {
                    Ok(initial_renderer) => {
                        let _ = ready_tx.send(Ok(()));
                        let mut renderer = initial_renderer;
                        let mut current_monitor = initial_monitor;
                        let mut last_check_gen = 0u64;
                        let mut retry_frame = false;
                        let mut minimized = false;
                        let mut logged_frame_error = false;
                        let mut consecutive_frame_errors = 0u32;
                        loop {
                            // A paused frame may have no mpv wake when the window returns.
                            if retry_frame || minimized {
                                match rx.recv_timeout(Duration::from_millis(100)) {
                                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                }
                            } else if rx.recv().is_err() {
                                break;
                            }
                            let layout = {
                                #[cfg(any(debug_assertions, feature = "devtools"))]
                                {
                                    let mut g =
                                        shared.lock().unwrap_or_else(PoisonError::into_inner);
                                    let s = *g;
                                    g.force_reset = false;
                                    s
                                }
                                #[cfg(not(any(debug_assertions, feature = "devtools")))]
                                {
                                    *shared.lock().unwrap_or_else(PoisonError::into_inner)
                                }
                            };
                            if layout.stopped {
                                break;
                            }
                            minimized = unsafe { IsIconic(raw_hwnd).as_bool() };
                            // Read the monitor's profile off the UI thread, then queue the mpv update.
                            if layout.monitor_check != last_check_gen {
                                last_check_gen = layout.monitor_check;
                                let m = monitor_of(raw_hwnd);
                                if m != current_monitor {
                                    current_monitor = m;
                                    update_display_profile(m, &render_mpv);
                                }
                            }
                            let draw_res = {
                                #[cfg(any(debug_assertions, feature = "devtools"))]
                                if layout.force_reset {
                                    Err(windows::core::Error::from(DXGI_ERROR_DEVICE_REMOVED)
                                        .into())
                                } else {
                                    unsafe { renderer.draw(layout, minimized) }
                                }
                                #[cfg(not(any(debug_assertions, feature = "devtools")))]
                                unsafe {
                                    renderer.draw(layout, minimized)
                                }
                            };
                            match draw_res {
                                Ok(()) => {
                                    retry_frame = false;
                                    logged_frame_error = false;
                                    consecutive_frame_errors = 0;
                                }
                                Err(e)
                                    if is_device_lost(&e, &renderer._device)
                                        || consecutive_frame_errors >= 2 =>
                                {
                                    let lost = is_device_lost(&e, &renderer._device);
                                    let err_str = format!("{e:#}");
                                    let count_str = consecutive_frame_errors.to_string();
                                    logging::event_kv(
                                        EventLevel::Warn,
                                        "video",
                                        if lost {
                                            "device_lost"
                                        } else {
                                            "surface_rebuild"
                                        },
                                        &[
                                            ("backend", "d3d11"),
                                            ("consecutive_errors", &count_str),
                                            ("error", &err_str),
                                        ],
                                    );
                                    let saved_vid = capture_active_vid(
                                        &renderer.render,
                                        &render_mpv,
                                        &callback_wake,
                                        &shared,
                                        &rx,
                                    );
                                    drop(renderer);
                                    let Some(rebuilt) = rebuild_renderer(
                                        raw_hwnd,
                                        &render_mpv,
                                        &callback_wake,
                                        &shared,
                                        &rx,
                                    ) else {
                                        break;
                                    };
                                    current_monitor = monitor_of(raw_hwnd);
                                    update_display_profile(current_monitor, &render_mpv);
                                    let restored_vid = match saved_vid {
                                        Some(track) => match render_mpv.restore_vid(track) {
                                            Ok(restored) => restored,
                                            Err(e) => {
                                                log::warn!("queuing video track recovery: {e}");
                                                false
                                            }
                                        },
                                        None => false,
                                    };
                                    logging::event_kv(
                                        EventLevel::Info,
                                        "video",
                                        "surface_rebuilt",
                                        &[("backend", "d3d11")],
                                    );
                                    let _ = reset_tx.unbounded_send(VideoReset { restored_vid });
                                    renderer = rebuilt;
                                    retry_frame = false;
                                    logged_frame_error = false;
                                    consecutive_frame_errors = 0;
                                    let _ = callback_wake.try_send(());
                                }
                                Err(e) => {
                                    if !logged_frame_error {
                                        let err_str = format!("{e:#}");
                                        logging::event_kv(
                                            EventLevel::Warn,
                                            "video",
                                            "frame_error",
                                            &[("backend", "d3d11"), ("error", &err_str)],
                                        );
                                        logged_frame_error = true;
                                    }
                                    consecutive_frame_errors =
                                        consecutive_frame_errors.saturating_add(1);
                                    renderer.layout = None;
                                    retry_frame = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                if initialized {
                    unsafe { CoUninitialize() };
                }
            },
        )?;
        match ready_rx
            .recv()
            .context("video initialization thread ended")?
        {
            Ok(()) => Ok((
                Self {
                    layout,
                    wake,
                    worker: Some(worker),
                    scale: Cell::new(scale),
                    last_monitor_check: Instant::now(),
                },
                reset_rx,
            )),
            Err(e) => {
                let _ = worker.join();
                Err(e)
            }
        }
    }

    pub(crate) fn request_redraw(&self) {
        let _ = self.wake.try_send(());
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub(crate) fn debug_force_device_reset(&self) -> bool {
        self.layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .force_reset = true;
        self.request_redraw();
        true
    }

    pub(crate) fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        let mut layout = self.layout.lock().unwrap_or_else(PoisonError::into_inner);
        let rect = [x, y, w, h];
        if layout.rect == rect {
            return;
        }
        layout.rect = rect;
        drop(layout);
        self.request_redraw();
    }

    pub(crate) fn set_scale(&mut self, scale: f64) {
        let scale_valid = scale.is_finite() && scale > 0.;
        let scale_changed = scale_valid && self.scale.replace(scale) != scale;
        let poll_monitor = self.last_monitor_check.elapsed() >= MONITOR_POLL_INTERVAL;
        if !scale_changed && !poll_monitor {
            return;
        }
        if poll_monitor {
            self.last_monitor_check = Instant::now();
        }
        let mut layout = self.layout.lock().unwrap_or_else(PoisonError::into_inner);
        if scale_changed {
            layout.scale = scale;
        }
        layout.monitor_check = layout.monitor_check.wrapping_add(1);
        drop(layout);
        self.request_redraw();
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.layout
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stopped = true;
            self.request_redraw();
            if worker.join().is_err() {
                log::error!("video worker panicked");
            }
        }
    }
}

impl Drop for VideoSurface {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn capture_active_vid(
    render: &RenderContext,
    mpv: &RenderHandle,
    wake: &mpsc::SyncSender<()>,
    layout: &Mutex<Layout>,
    rx: &mpsc::Receiver<()>,
) -> Option<ActiveVideoTrack> {
    let reply = match mpv.request_active_vid(wake.clone()) {
        Ok(Some(reply)) => reply,
        Ok(None) => return None,
        Err(e) => {
            log::warn!("queuing video track query: {e}");
            return None;
        }
    };
    let deadline = Instant::now() + VID_CAPTURE_TIMEOUT;
    loop {
        if layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped
        {
            return None;
        }
        match reply.try_recv() {
            Ok(track) => return track,
            Err(mpsc::TryRecvError::Disconnected) => return None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log::warn!("timed out waiting for active video track before surface rebuild");
            return None;
        }
        // The core may need this context before it can answer the query.
        if let Err(e) = skip_render_update(render) {
            log::warn!("servicing lost video device: {e}");
        }
        match rx.recv_timeout(remaining) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn skip_render_update(render: &RenderContext) -> iris_mpv::Result<bool> {
    // SAFETY: This is D3D11, on the only thread that uses this render context.
    if unsafe { render.update() } & RenderContext::UPDATE_FRAME == 0 {
        return Ok(false);
    }
    unsafe { render.skip_frame()? };
    render.report_swap();
    Ok(true)
}

fn rebuild_renderer(
    hwnd: HWND,
    mpv: &RenderHandle,
    wake: &mpsc::SyncSender<()>,
    layout: &Mutex<Layout>,
    rx: &mpsc::Receiver<()>,
) -> Option<Renderer> {
    let mut delay = Duration::from_millis(50);
    loop {
        let deadline = Instant::now() + delay;
        loop {
            if layout
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stopped
            {
                return None;
            }
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Ok(()) if Instant::now() >= deadline => break,
                Ok(()) => {}
            }
        }
        if layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped
        {
            return None;
        }
        match unsafe { Renderer::new(hwnd, mpv.clone(), wake.clone()) } {
            Ok(renderer) => return Some(renderer),
            Err(e) => log::warn!("recreating D3D11 surface failed: {e:#}"),
        }
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

fn is_device_lost(err: &anyhow::Error, device: &ID3D11Device) -> bool {
    if unsafe { device.GetDeviceRemovedReason() }.is_err() {
        return true;
    }
    err.downcast_ref::<windows::core::Error>().is_some_and(|e| {
        let code = e.code();
        matches!(
            code,
            DXGI_ERROR_DEVICE_REMOVED
                | DXGI_ERROR_DEVICE_RESET
                | DXGI_ERROR_DEVICE_HUNG
                | DXGI_ERROR_DRIVER_INTERNAL_ERROR
        )
    })
}

fn monitor_of(hwnd: HWND) -> isize {
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize
}

struct Renderer {
    hwnd: HWND,
    render: RenderContext,
    swap: IDXGISwapChain1,
    comp: IDCompositionDevice,
    target: IDCompositionTarget,
    visual: IDCompositionVisual,
    // Keeping both objects is intentional: this is not GPUI's device/context.
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
    layout: Option<Layout>,
    size: (u32, u32),
    has_frame: bool,
}

impl Renderer {
    unsafe fn new(hwnd: HWND, mpv: RenderHandle, wake: mpsc::SyncSender<()>) -> Result<Self> {
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.context("D3D11 device missing")?;
        let context = context.context("D3D11 context missing")?;
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 1,
            Height: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swap = unsafe { factory.CreateSwapChainForComposition(&device, &desc, None)? };
        unsafe { clear_swap_black(&device, &context, &swap)? };
        let comp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi)? };
        let target = unsafe { comp.CreateTargetForHwnd(hwnd, false)? };
        let visual = unsafe { comp.CreateVisual()? };
        unsafe {
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            comp.Commit()?;
        }
        let mut render = unsafe { RenderContext::new_d3d11(mpv, device.as_raw()) }
            .context("creating mpv D3D11 render context (requires a D3D11-enabled libmpv)")?;
        render.set_update_callback(move || {
            let _ = wake.try_send(());
        });
        logging::event_kv(
            EventLevel::Info,
            "video",
            "surface_attached",
            &[("backend", "d3d11")],
        );
        Ok(Self {
            hwnd,
            render,
            swap,
            comp,
            target,
            visual,
            _device: device,
            _context: context,
            layout: None,
            size: (1, 1),
            has_frame: false,
        })
    }

    unsafe fn draw(&mut self, layout: Layout, minimized: bool) -> Result<()> {
        let valid_geometry = layout.rect.iter().all(|v| v.is_finite())
            && layout.scale.is_finite()
            && layout.scale > 0.
            && layout.rect[2] > 0.
            && layout.rect[3] > 0.;
        if !valid_geometry || minimized {
            // Keep the latest image in mpv's current frame and redraw it with
            // the restored geometry even if no new frame arrives.
            self.layout = None;
            self.has_frame |= skip_render_update(&self.render)?;
            return Ok(());
        }
        let geometry_changed = self
            .layout
            .is_none_or(|prev| prev.rect != layout.rect || prev.scale != layout.scale);
        // SAFETY: This is D3D11 and runs on the sole render worker.
        let update = unsafe { self.render.update() };
        let new_frame = update & RenderContext::UPDATE_FRAME != 0;
        if !new_frame && !geometry_changed {
            return Ok(());
        }
        let client_size = client_pixel_size(self.hwnd);
        let (ox, oy, size) = physical_rect(layout.rect, layout.scale, client_size);
        if size != self.size {
            unsafe {
                // Ensure no swapchain render target remains bound before resizing.
                self._context.OMSetRenderTargets(None, None);
                self._context.Flush();
                self.swap.ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
            }
            self.size = size;
        }
        if new_frame || self.has_frame {
            let texture: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
            unsafe {
                self.render.render_d3d11(texture.as_raw())?;
            }
            drop(texture);
            self.has_frame = true;
            unsafe {
                self.swap.Present(1, Default::default()).ok()?;
            }
            self.render.report_swap();
        } else {
            // Keep an opaque black backing surface sized to the viewport before
            // the first video frame arrives so WS_EX_NOREDIRECTIONBITMAP never
            // punches through to the desktop wallpaper.
            unsafe { clear_swap_black(&self._device, &self._context, &self.swap)? };
        }
        // Commit visual offset *after* presenting the resized buffer so DComp
        // does not briefly display a stale-sized frame at the new offset.
        if geometry_changed {
            unsafe {
                self.visual.SetOffsetX2(ox)?;
                self.visual.SetOffsetY2(oy)?;
                self.comp.Commit()?;
            }
            self.layout = Some(layout);
        }
        Ok(())
    }
}

fn client_pixel_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_ok()
        && rect.right > rect.left
        && rect.bottom > rect.top
    {
        Some((
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    } else {
        None
    }
}

fn physical_rect(
    rect: [f64; 4],
    scale: f64,
    client_size: Option<(u32, u32)>,
) -> (f32, f32, (u32, u32)) {
    // Epsilon absorbs f32->f64 logical-to-physical round-trip noise (e.g.
    // 2160 / 1.75 as f32 * 1.75 = 2160.00009155) without clipping real subpixels.
    const EPS: f64 = 1e-3;
    let ox = (rect[0] * scale + EPS)
        .floor()
        .clamp(-1_000_000., 1_000_000.);
    let oy = (rect[1] * scale + EPS)
        .floor()
        .clamp(-1_000_000., 1_000_000.);
    let mut right = ((rect[0] + rect[2]) * scale - EPS)
        .ceil()
        .clamp(ox + 1., ox + MAX_TEXTURE_DIM);
    let mut bottom = ((rect[1] + rect[3]) * scale - EPS)
        .ceil()
        .clamp(oy + 1., oy + MAX_TEXTURE_DIM);
    if let Some((cw, ch)) = client_size
        && cw > 0
        && ch > 0
    {
        let max_x = cw as f64;
        let max_y = ch as f64;
        if ox >= 0. && ox < max_x {
            right = right.min(max_x).max(ox + 1.);
        }
        if oy >= 0. && oy < max_y {
            bottom = bottom.min(max_y).max(oy + 1.);
        }
    }
    (
        ox as f32,
        oy as f32,
        ((right - ox) as u32, (bottom - oy) as u32),
    )
}

unsafe fn clear_swap_black(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    unsafe {
        {
            let texture: ID3D11Texture2D = swap.GetBuffer(0)?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device.CreateRenderTargetView(&texture, None, Some(&mut rtv))?;
            if let Some(rtv) = rtv {
                context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
        swap.Present(0, Default::default()).ok()?;
    }
    Ok(())
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.target.SetRoot(None);
            let _ = self.comp.Commit();
        }
    }
}

fn update_display_profile(monitor: isize, mpv: &RenderHandle) {
    let result = monitor_profile(monitor).and_then(|path| {
        mpv.set_display_icc_profile(&path)?;
        if !path.is_empty() {
            log::info!("display ICC queued: {path}");
        }
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("display ICC unavailable, using SDR sRGB: {e:#}");
        if let Err(e) = mpv.set_display_icc_profile("") {
            log::warn!("clearing display ICC profile: {e}");
        }
    }
}

fn monitor_profile(monitor: isize) -> Result<String> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::ColorSystem::GetICMProfileW;
    use windows::core::{PCWSTR, PWSTR};
    unsafe {
        let monitor = HMONITOR(monitor as *mut _);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        GetMonitorInfoW(monitor, &mut info.monitorInfo).ok()?;
        let dc = CreateDCW(
            PCWSTR::null(),
            PCWSTR(info.szDevice.as_ptr()),
            PCWSTR::null(),
            None,
        );
        anyhow::ensure!(!dc.0.is_null(), "creating monitor color context failed");
        let mut length = 0;
        let _ = GetICMProfileW(dc, &mut length, PWSTR::null());
        let mut profile = vec![0u16; length as usize];
        let found =
            length > 0 && GetICMProfileW(dc, &mut length, PWSTR(profile.as_mut_ptr())).as_bool();
        let _ = DeleteDC(dc);
        if found {
            let end = profile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(profile.len());
            Ok(String::from_utf16(&profile[..end])?)
        } else {
            Ok(String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::physical_rect;

    #[test]
    fn physical_rect_avoids_fractional_dpi_overflow_in_4k_fullscreen() {
        // Real 4K (3840x2160) at 175% DPI logical bounds from GPUI f32->f64 round-trip.
        let (ox, oy, size) = physical_rect(
            [0.0, 0.0, 2194.28564453125, 1234.2857666015625],
            1.75,
            Some((3840, 2160)),
        );
        assert_eq!(ox, 0.0);
        assert_eq!(oy, 0.0);
        assert_eq!(size, (3840, 2160));
    }

    #[test]
    fn physical_rect_clamps_to_client_bounds_and_preserves_subpixel_coverage() {
        let (ox, oy, size) = physical_rect([0.0, 38.0, 1000.0, 600.0], 1.75, Some((3840, 2160)));
        assert_eq!(ox, 0.0);
        assert_eq!(oy, 66.0);
        assert_eq!(size, (1750, 1051));

        let (_, _, clamped) = physical_rect([0.0, 0.0, 2500.0, 1500.0], 1.75, Some((3840, 2160)));
        assert_eq!(clamped, (3840, 2160));
    }
}
