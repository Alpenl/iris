//! Independent D3D11 device, composed below GPUI on the same HWND.
//! Only the render worker touches its immediate context and mpv render API.

use std::cell::Cell;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::render::{RenderContext, RenderHandle};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HMODULE, HWND},
    Graphics::{
        Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
            ID3D11DeviceContext, ID3D11Texture2D,
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
};
use windows::core::Interface;

const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
    ) -> Result<(Self, UnboundedReceiver<()>)> {
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
        let initial_monitor = update_display_profile(HWND(hwnd as *mut _), &mpv)?;
        let render_mpv = mpv.clone();
        let scale = window.scale_factor() as f64;
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
        let worker = std::thread::Builder::new()
            .name("iris-video".into())
            .spawn(move || {
                // COM and every D3D/render resource remain on this thread.
                let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                let raw_hwnd = HWND(hwnd as *mut _);
                let renderer = unsafe {
                    Renderer::new(raw_hwnd, render_mpv.clone(), callback_wake.clone())
                };
                match renderer {
                    Ok(initial_renderer) => {
                        let _ = ready_tx.send(Ok(()));
                        let mut renderer = Some(initial_renderer);
                        let mut current_monitor = initial_monitor;
                        let mut last_check_gen = 0u64;
                        let mut consecutive_resets = 0u8;
                        while rx.recv().is_ok() {
                            let layout = {
                                #[cfg(any(debug_assertions, feature = "devtools"))]
                                {
                                    let mut g = shared.lock().unwrap();
                                    let s = *g;
                                    g.force_reset = false;
                                    s
                                }
                                #[cfg(not(any(debug_assertions, feature = "devtools")))]
                                {
                                    *shared.lock().unwrap()
                                }
                            };
                            if layout.stopped {
                                break;
                            }
                            // Check display ICC off the UI thread so Little CMS LUT rebuilds never stall UI.
                            if layout.monitor_check != last_check_gen {
                                last_check_gen = layout.monitor_check;
                                let m = monitor_of(raw_hwnd);
                                if m != current_monitor {
                                    match update_display_profile(raw_hwnd, &render_mpv) {
                                        Ok(updated) => current_monitor = updated,
                                        Err(e) => {
                                            log::error!("updating display color profile: {e:#}")
                                        }
                                    }
                                }
                            }
                            let Some(active) = renderer.as_mut() else {
                                break;
                            };
                            let draw_res = {
                                #[cfg(any(debug_assertions, feature = "devtools"))]
                                if layout.force_reset {
                                    Err(windows::core::Error::from(DXGI_ERROR_DEVICE_REMOVED)
                                        .into())
                                } else {
                                    unsafe { active.draw(layout) }
                                }
                                #[cfg(not(any(debug_assertions, feature = "devtools")))]
                                unsafe {
                                    active.draw(layout)
                                }
                            };
                            match draw_res {
                                Ok(()) => {
                                    consecutive_resets = 0;
                                }
                                Err(e) => {
                                    if is_device_lost(&e, &active._device) && consecutive_resets < 4
                                    {
                                        consecutive_resets += 1;
                                        log::warn!(
                                            "D3D11 device lost ({e:#}); recreating video surface (attempt {consecutive_resets})"
                                        );
                                        let saved_vid = render_mpv.active_vid();
                                        renderer = None;
                                        std::thread::sleep(Duration::from_millis(50));
                                        match unsafe {
                                            Renderer::new(
                                                raw_hwnd,
                                                render_mpv.clone(),
                                                callback_wake.clone(),
                                            )
                                        } {
                                            Ok(rebuilt) => {
                                                if let Ok(m) =
                                                    update_display_profile(raw_hwnd, &render_mpv)
                                                {
                                                    current_monitor = m;
                                                }
                                                if let Some(vid) = saved_vid.as_deref() {
                                                    let _ = render_mpv.restore_vid(vid);
                                                }
                                                let _ = reset_tx.unbounded_send(());
                                                renderer = Some(rebuilt);
                                                let _ = callback_wake.try_send(());
                                                continue;
                                            }
                                            Err(re) => {
                                                log::error!(
                                                    "recreating D3D11 surface failed: {re:#}"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                    log::error!("video rendering stopped: {e:#}");
                                    break;
                                }
                            }
                        }
                        // Free the mpv render context here, before the application
                        // sends quit to the client API, never from its callback.
                        drop(renderer.take());
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                if initialized {
                    unsafe { CoUninitialize() };
                }
            })?;
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
    pub(crate) fn debug_force_device_reset(&self) {
        self.layout.lock().unwrap().force_reset = true;
        self.request_redraw();
    }

    pub(crate) fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        let mut layout = self.layout.lock().unwrap();
        let rect = [x, y, w, h];
        if layout.rect == rect {
            return;
        }
        layout.rect = rect;
        drop(layout);
        self.request_redraw();
    }

    pub(crate) fn set_scale(&mut self, scale: f64) {
        let scale_changed = self.scale.replace(scale) != scale;
        let poll_monitor = self.last_monitor_check.elapsed() >= MONITOR_POLL_INTERVAL;
        if !scale_changed && !poll_monitor {
            return;
        }
        if poll_monitor {
            self.last_monitor_check = Instant::now();
        }
        let mut layout = self.layout.lock().unwrap();
        if scale_changed {
            layout.scale = scale;
        }
        layout.monitor_check = layout.monitor_check.wrapping_add(1);
        drop(layout);
        self.request_redraw();
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.layout.lock().unwrap().stopped = true;
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
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swap = unsafe { factory.CreateSwapChainForComposition(&device, &desc, None)? };
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
        log::info!("Windows D3D11 surface attached below GPUI");
        Ok(Self {
            render,
            swap,
            comp,
            target,
            visual,
            _device: device,
            _context: context,
            layout: None,
            size: (1, 1),
        })
    }

    unsafe fn draw(&mut self, layout: Layout) -> Result<()> {
        let geometry_changed = self
            .layout
            .is_none_or(|prev| prev.rect != layout.rect || prev.scale != layout.scale);
        let update = self.render.update();
        if !geometry_changed && update & RenderContext::UPDATE_FRAME == 0 {
            return Ok(());
        }
        if layout.rect[2] <= 0. || layout.rect[3] <= 0. {
            return Ok(());
        }
        let size = (
            (layout.rect[2] * layout.scale).round().max(1.) as u32,
            (layout.rect[3] * layout.scale).round().max(1.) as u32,
        );
        if size != self.size {
            unsafe {
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
        let texture: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        unsafe {
            self.render.render_d3d11(texture.as_raw())?;
        }
        unsafe {
            self.swap.Present(1, Default::default()).ok()?;
        }
        self.render.report_swap();
        // Commit visual offset *after* presenting the resized buffer so DComp
        // does not briefly display a stale-sized frame at the new offset.
        if geometry_changed {
            unsafe {
                self.visual
                    .SetOffsetX2((layout.rect[0] * layout.scale) as f32)?;
                self.visual
                    .SetOffsetY2((layout.rect[1] * layout.scale) as f32)?;
                self.comp.Commit()?;
            }
            self.layout = Some(layout);
        }
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.target.SetRoot(None);
            let _ = self.comp.Commit();
        }
    }
}

fn update_display_profile(hwnd: HWND, mpv: &RenderHandle) -> Result<isize> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::ColorSystem::GetICMProfileW;
    use windows::core::{PCWSTR, PWSTR};
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
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
            let path = String::from_utf16(&profile[..end])?;
            mpv.set_display_icc_profile(&path)?;
            log::info!("display ICC: {path}");
        } else {
            mpv.set_display_icc_profile("")?;
            log::info!("display has no ICC profile; using SDR sRGB output");
        }
        Ok(monitor.0 as isize)
    }
}
