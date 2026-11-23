//! libmpv render API (OpenGL and Direct3D 11).
//!
//! Rules from render.h that this wrapper relies on:
//! - The update callback may run on any mpv thread and must only signal.
//! - Render calls are serialized; OpenGL additionally requires its context current.
//! - The render context must be freed before the `Mpv` handle is destroyed;
//!   `RenderContext` keeps an `Arc<Mpv>` so the handle outlives it.

use std::ffi::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

use crate::{Error, Mpv, Result, check, sys};

pub type GetProcAddress =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;

/// Capability-restricted handle granting video surface workers access ONLY to
/// render context creation and display color-space target properties.
#[derive(Clone)]
pub struct RenderHandle(Arc<Mpv>);

impl RenderHandle {
    pub fn new(mpv: Arc<Mpv>) -> Self {
        Self(mpv)
    }

    pub fn set_display_color_target(&self, prim: &str, trc: &str) -> Result<()> {
        self.0.set_property_string("target-prim", prim)?;
        self.0.set_property_string("target-trc", trc)
    }

    pub fn set_display_icc_profile(&self, path: &str) -> Result<()> {
        self.0.set_property_string("icc-profile", path)
    }

    /// Returns the currently selected concrete video track id (e.g. `"1"`), if any.
    pub fn active_vid(&self) -> Option<String> {
        self.0
            .get_property_string("vid")
            .ok()
            .filter(|v| !v.is_empty() && v != "no" && v != "auto")
    }

    /// Re-attaches `vid` after recreating a GPU render context so hardware decoding
    /// (`d3d11va`) and `vo=libmpv` bind to the new device. Returns `false` if `mpv`
    /// unloaded the file while the VO was closed (e.g. still images or silent clips).
    pub fn restore_vid(&self, vid: &str) -> bool {
        if self
            .0
            .get_property_string("path")
            .is_ok_and(|p| !p.is_empty())
        {
            let _ = self.0.set_property_string("vid", vid);
            true
        } else {
            false
        }
    }
}

type UpdateFn = Box<dyn Fn() + Send + Sync>;

pub struct RenderContext {
    ctx: *mut sys::mpv_render_context,
    update: Option<ptr::NonNull<UpdateFn>>,
    _mpv: Arc<Mpv>,
}

// SAFETY: `mpv_render_context` may be moved between threads as long as render calls are serialized.
unsafe impl Send for RenderContext {}

impl RenderContext {
    pub const UPDATE_FRAME: u64 = sys::MPV_RENDER_UPDATE_FRAME;

    /// Creates an OpenGL render context. The GL context must be current.
    ///
    /// # Safety
    /// `get_proc_address` must resolve GL symbols for the current context.
    /// The caller must also make the same GL context current when dropping the
    /// returned render context.
    pub unsafe fn new_gl(
        mpv: RenderHandle,
        get_proc_address: GetProcAddress,
        proc_ctx: *mut c_void,
    ) -> Result<Self> {
        let mut init = sys::mpv_opengl_init_params {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: proc_ctx,
        };
        let mut advanced: c_int = 1;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced as *mut c_int as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let mut ctx = ptr::null_mut();
        // SAFETY: `mpv.0` holds a live `mpv_handle`, `params` is `MPV_RENDER_PARAM_INVALID`-terminated,
        // and the caller guarantees a current GL context matching `get_proc_address`.
        check(
            unsafe { sys::mpv_render_context_create(&mut ctx, mpv.0.raw(), params.as_mut_ptr()) },
            || "mpv_render_context_create(opengl)".into(),
        )?;
        if ctx.is_null() {
            return Err(Error {
                code: -1,
                context: "render context is null".into(),
            });
        }
        Ok(Self {
            ctx,
            update: None,
            _mpv: mpv.0,
        })
    }

    /// Creates a D3D11 render context on the caller's independent device.
    ///
    /// # Safety
    /// `device` must be a live ID3D11Device without SINGLETHREADED. Its immediate
    /// context must not be used concurrently with this render context.
    pub unsafe fn new_d3d11(mpv: RenderHandle, device: *mut c_void) -> Result<Self> {
        let mut init = sys::mpv_d3d11_init_params { device };
        let mut advanced: c_int = 1;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_API_TYPE,
                data: sys::MPV_RENDER_API_TYPE_D3D11.as_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_D3D11_INIT_PARAMS,
                data: &mut init as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: 0,
                data: ptr::null_mut(),
            },
        ];
        let mut ctx = ptr::null_mut();
        // SAFETY: `mpv.0` holds a live `mpv_handle`, `params` is zero-terminated,
        // and the caller guarantees `device` is a valid multi-thread-safe ID3D11Device.
        check(
            unsafe { sys::mpv_render_context_create(&mut ctx, mpv.0.raw(), params.as_mut_ptr()) },
            || "mpv_render_context_create(d3d11)".into(),
        )?;
        if ctx.is_null() {
            return Err(Error {
                code: -1,
                context: "render context is null".into(),
            });
        }
        Ok(Self {
            ctx,
            update: None,
            _mpv: mpv.0,
        })
    }

    /// # Safety
    /// `texture` must be a non-MSAA, single-mip ID3D11Texture2D render target on
    /// this context's device, and must stay alive until this call returns.
    pub unsafe fn render_d3d11(&self, texture: *mut c_void) -> Result<()> {
        let mut fbo = sys::mpv_d3d11_fbo { tex: texture };
        let mut block: c_int = 0;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_D3D11_FBO,
                data: &mut fbo as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: 0,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: `self.ctx` is valid, `params` is zero-terminated, and the caller
        // guarantees `texture` is a valid D3D11 render target for the duration of the call.
        check(
            unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) },
            || "mpv_render_context_render(d3d11)".into(),
        )
    }

    /// Sets the callback mpv calls when a new frame (or redraw) is wanted.
    /// It runs on an mpv thread: only signal from it, never call mpv.
    pub fn set_update_callback(&mut self, f: impl Fn() + Send + Sync + 'static) {
        unsafe extern "C" fn trampoline(data: *mut c_void) {
            // SAFETY: Registration owns this allocation until mpv replaces the
            // callback under its update lock or synchronously unregisters it.
            let f = unsafe { &*data.cast::<UpdateFn>() };
            if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
                // A custom panic payload can itself panic when dropped.
                std::mem::forget(payload);
            }
        }
        let callback: UpdateFn = Box::new(f);
        let data = Box::into_raw(Box::new(callback));
        // Register only after releasing ownership into a raw pointer. libmpv can
        // invoke the new callback before this function returns.
        unsafe {
            sys::mpv_render_context_set_update_callback(self.ctx, Some(trampoline), data.cast())
        };
        // SAFETY: libmpv has replaced and finished using the previous callback.
        if let Some(old) = self.update.replace(ptr::NonNull::new(data).unwrap()) {
            unsafe { drop(Box::from_raw(old.as_ptr())) };
        }
    }

    /// Returns `MPV_RENDER_UPDATE_*` flags. Call before deciding whether to render.
    pub fn update(&self) -> u64 {
        // SAFETY: `self.ctx` is a valid `mpv_render_context`.
        unsafe { sys::mpv_render_context_update(self.ctx) }
    }

    /// Renders the current frame into the given GL framebuffer.
    ///
    /// # Safety
    /// The GL context used to create this render context must be current, and
    /// the framebuffer must be a valid target in that context.
    pub unsafe fn render_gl(
        &self,
        fbo: i32,
        w: i32,
        h: i32,
        flip_y: bool,
        block_for_target: bool,
    ) -> Result<()> {
        let mut target = sys::mpv_opengl_fbo {
            fbo,
            w,
            h,
            internal_format: 0,
        };
        let mut flip: c_int = flip_y as c_int;
        let mut block: c_int = block_for_target as c_int;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut target as *mut _ as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip as *mut c_int as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut c_int as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: `self.ctx` is valid and `params` is `MPV_RENDER_PARAM_INVALID`-terminated.
        check(
            unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) },
            || "mpv_render_context_render".into(),
        )
    }

    /// Tells mpv a frame was presented (improves frame timing).
    pub fn report_swap(&self) {
        // SAFETY: `self.ctx` is a valid `mpv_render_context`.
        unsafe { sys::mpv_render_context_report_swap(self.ctx) }
    }
}

impl Drop for RenderContext {
    /// OpenGL callers must keep their GL context current when this runs.
    fn drop(&mut self) {
        // SAFETY: Detaches the update callback before freeing `self.ctx`, while `_mpv`
        // and `self.update` are still alive.
        unsafe {
            sys::mpv_render_context_set_update_callback(self.ctx, None, ptr::null_mut());
            sys::mpv_render_context_free(self.ctx);
        }
        if let Some(callback) = self.update.take() {
            unsafe { drop(Box::from_raw(callback.as_ptr())) };
        }
    }
}
