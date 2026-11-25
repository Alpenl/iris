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
#[cfg(target_os = "windows")]
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use crate::{Error, Mpv, Result, check, sys};
#[cfg(target_os = "windows")]
use crate::{Event, with_cstr};

pub type GetProcAddress =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void;

/// Capability-restricted handle for render context creation, display color
/// management, and video track recovery after a device reset.
#[derive(Clone)]
pub struct RenderHandle(Arc<Mpv>);

impl RenderHandle {
    pub fn new(mpv: Arc<Mpv>) -> Self {
        Self(mpv)
    }

    /// Configure output before creating a render context for this player.
    /// These synchronous calls must not run on an active render thread.
    pub fn set_display_color_target(&self, prim: &str, trc: &str) -> Result<()> {
        self.0.set_property_string("target-prim", prim)?;
        self.0.set_property_string("target-trc", trc)
    }

    /// Queues a display profile change. Safe on an active render thread.
    pub fn set_display_icc_profile(&self, path: &str) -> Result<()> {
        self.0.set_property_string_async("icc-profile", path)
    }

    /// Requests the selected concrete video track before freeing a lost device.
    /// The client event loop delivers the result and signals `wake`. Keep
    /// servicing render updates while waiting for it.
    #[cfg(target_os = "windows")]
    pub fn request_active_vid(
        &self,
        wake: SyncSender<()>,
    ) -> Result<Receiver<Option<ActiveVideoTrack>>> {
        let (tx, rx) = mpsc::channel();
        let mut recovery = self
            .0
            .video_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(entry) = recovery.entry else {
            return Ok(rx);
        };
        if recovery.pending.is_some() {
            return Err(Error {
                code: sys::MPV_ERROR_INVALID_PARAMETER,
                context: "video track query already pending".into(),
            });
        }
        with_cstr("vid", |name| {
            // SAFETY: The live handle queues a copy of the property name.
            check(
                unsafe {
                    sys::mpv_get_property_async(
                        self.0.raw(),
                        VideoRecovery::QUERY_VID,
                        name,
                        sys::MPV_FORMAT_STRING,
                    )
                },
                || "get_property_async vid".into(),
            )
        })?;
        recovery.pending = Some(VideoTrackRequest { entry, tx, wake });
        Ok(rx)
    }

    /// Queues video reattachment after recreating the render context, provided
    /// the corresponding file is still active in the client event stream.
    #[cfg(target_os = "windows")]
    pub fn restore_vid(&self, track: ActiveVideoTrack) -> Result<()> {
        let recovery = self
            .0
            .video_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if recovery.entry == Some(track.entry) {
            self.0
                .set_property_string_async("vid", &track.id.to_string())?;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub struct ActiveVideoTrack {
    entry: i64,
    id: i64,
}

#[cfg(target_os = "windows")]
struct VideoTrackRequest {
    entry: i64,
    tx: Sender<Option<ActiveVideoTrack>>,
    wake: SyncSender<()>,
}

#[cfg(target_os = "windows")]
#[derive(Default)]
pub(crate) struct VideoRecovery {
    entry: Option<i64>,
    pending: Option<VideoTrackRequest>,
}

#[cfg(target_os = "windows")]
impl VideoRecovery {
    pub(crate) const QUERY_VID: u64 = 1;

    pub(crate) fn on_event(&mut self, event: &Event) {
        match *event {
            Event::StartFile(entry) => self.entry = Some(entry),
            Event::EndFile {
                playlist_entry_id, ..
            } if self.entry == Some(playlist_entry_id) => {
                self.entry = None;
            }
            Event::QueueOverflow | Event::Shutdown => {
                self.entry = None;
                self.reply(None);
            }
            _ => (),
        }
    }

    pub(crate) fn set_entry(&mut self, entry: Option<i64>) {
        self.entry = entry;
    }

    pub(crate) fn reply(&mut self, vid: Option<i64>) {
        if let Some(VideoTrackRequest { entry, tx, wake }) = self.pending.take() {
            let track = vid
                .filter(|_| self.entry == Some(entry))
                .map(|id| ActiveVideoTrack { entry, id });
            let _ = tx.send(track);
            let _ = wake.try_send(());
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
#[test]
fn recovery_discards_track_from_replaced_file() {
    let (wake, woke) = mpsc::sync_channel(1);
    let (tx, rx) = mpsc::channel();
    let mut recovery = VideoRecovery {
        entry: Some(7),
        pending: Some(VideoTrackRequest { entry: 7, tx, wake }),
    };
    recovery.on_event(&Event::StartFile(8));
    recovery.reply(Some(3));
    woke.try_recv().expect("video worker notified");
    assert!(rx.try_recv().expect("query completed").is_none());

    let (wake2, woke2) = mpsc::sync_channel(1);
    let (tx2, rx2) = mpsc::channel();
    let mut overflow = VideoRecovery {
        entry: Some(8),
        pending: Some(VideoTrackRequest {
            entry: 8,
            tx: tx2,
            wake: wake2,
        }),
    };
    overflow.on_event(&Event::QueueOverflow);
    woke2.try_recv().expect("overflow unblocks video worker");
    assert!(rx2.try_recv().expect("overflow completes query").is_none());
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

    /// Services advanced-control requests and returns `MPV_RENDER_UPDATE_*` flags.
    /// Call after each update notification, including when no drawable is available.
    ///
    /// # Safety
    /// For OpenGL, the context used to create this render context must be current.
    pub unsafe fn update(&self) -> u64 {
        // SAFETY: `self.ctx` is valid and the caller guarantees the OpenGL context if needed.
        unsafe { sys::mpv_render_context_update(self.ctx) }
    }

    /// Advances the current frame without drawing to a target. mpv holds the
    /// image for the next redraw. Follow a successful skip with `report_swap`
    /// if the context normally reports frame presentations.
    ///
    /// # Safety
    /// For OpenGL, the same GL context used to create this render context must be current.
    pub unsafe fn skip_frame(&self) -> Result<()> {
        let mut skip: c_int = 1;
        let mut block: c_int = 0;
        let mut params = [
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_SKIP_RENDERING,
                data: (&mut skip as *mut c_int).cast(),
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&mut block as *mut c_int).cast(),
            },
            sys::mpv_render_param {
                type_: sys::MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: The caller supplies the matching GL context if required, and
        // the terminated parameters request a skip without any target surface.
        check(
            unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) },
            || "mpv_render_context_render(skip)".into(),
        )
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
