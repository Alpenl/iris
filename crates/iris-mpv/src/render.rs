//! libmpv render API (OpenGL and Direct3D 11).
//!
//! Rules from render.h that this wrapper relies on:
//! - The update callback may run on any mpv thread and must only signal.
//! - Render calls are serialized; OpenGL additionally requires its context current.
//! - The render context must be freed before the `Mpv` handle is destroyed;
//!   `RenderContext` keeps an `Arc<Mpv>` so the handle outlives it.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;

use crate::{Error, Mpv, Result, check, sys};

pub type GetProcAddress = unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;

type UpdateFn = Box<dyn Fn() + Send + Sync>;

pub struct RenderContext {
    ctx: *mut sys::mpv_render_context,
    update: Option<Box<UpdateFn>>,
    _mpv: Arc<Mpv>,
}

unsafe impl Send for RenderContext {}

impl RenderContext {
    /// Creates an OpenGL render context. The GL context must be current.
    ///
    /// # Safety
    /// `get_proc_address` must resolve GL symbols for the current context.
    pub unsafe fn new_gl(mpv: Arc<Mpv>, get_proc_address: GetProcAddress, proc_ctx: *mut c_void) -> Result<Self> {
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
            sys::mpv_render_param { type_: sys::MPV_RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        let mut ctx = ptr::null_mut();
        check(
            unsafe { sys::mpv_render_context_create(&mut ctx, mpv.raw(), params.as_mut_ptr()) },
            || "mpv_render_context_create(opengl)".into(),
        )?;
        if ctx.is_null() {
            return Err(Error { code: -1, context: "render context is null".into() });
        }
        Ok(Self { ctx, update: None, _mpv: mpv })
    }

    /// Creates a D3D11 render context on the caller's independent device.
    ///
    /// # Safety
    /// `device` must be a live ID3D11Device without SINGLETHREADED. Its immediate
    /// context must not be used concurrently with this render context.
    pub unsafe fn new_d3d11(mpv: Arc<Mpv>, device: *mut c_void) -> Result<Self> {
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
            sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
        ];
        let mut ctx = ptr::null_mut();
        check(unsafe { sys::mpv_render_context_create(&mut ctx, mpv.raw(), params.as_mut_ptr()) },
            || "mpv_render_context_create(d3d11)".into())?;
        if ctx.is_null() {
            return Err(Error { code: -1, context: "render context is null".into() });
        }
        Ok(Self { ctx, update: None, _mpv: mpv })
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
            sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
        ];
        check(unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) },
            || "mpv_render_context_render(d3d11)".into())
    }

    /// Sets the callback mpv calls when a new frame (or redraw) is wanted.
    /// It runs on an mpv thread: only signal from it, never call mpv.
    pub fn set_update_callback(&mut self, f: impl Fn() + Send + Sync + 'static) {
        unsafe extern "C" fn trampoline(data: *mut c_void) {
            let f = unsafe { &*(data as *const UpdateFn) };
            f();
        }
        let boxed: Box<UpdateFn> = Box::new(Box::new(f));
        let data = &*boxed as *const UpdateFn as *mut c_void;
        unsafe { sys::mpv_render_context_set_update_callback(self.ctx, Some(trampoline), data) };
        self.update = Some(boxed);
    }

    /// Returns `MPV_RENDER_UPDATE_*` flags. Call before deciding whether to render.
    pub fn update(&self) -> u64 {
        unsafe { sys::mpv_render_context_update(self.ctx) }
    }

    /// Renders the current frame into the given GL framebuffer.
    pub fn render_gl(&self, fbo: i32, w: i32, h: i32, flip_y: bool, block_for_target: bool) -> Result<()> {
        let mut target = sys::mpv_opengl_fbo { fbo, w, h, internal_format: 0 };
        let mut flip: c_int = flip_y as c_int;
        let mut block: c_int = block_for_target as c_int;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut target as *mut _ as *mut c_void,
            },
            sys::mpv_render_param { type_: sys::MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut c_int as *mut c_void },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut c_int as *mut c_void,
            },
            sys::mpv_render_param { type_: sys::MPV_RENDER_PARAM_INVALID, data: ptr::null_mut() },
        ];
        check(unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) }, || {
            "mpv_render_context_render".into()
        })
    }

    /// Tells mpv a frame was presented (improves frame timing).
    pub fn report_swap(&self) {
        unsafe { sys::mpv_render_context_report_swap(self.ctx) }
    }
}

impl Drop for RenderContext {
    /// OpenGL callers must keep their GL context current when this runs.
    fn drop(&mut self) {
        unsafe {
            sys::mpv_render_context_set_update_callback(self.ctx, None, ptr::null_mut());
            sys::mpv_render_context_free(self.ctx);
        }
        self.update = None;
    }
}
