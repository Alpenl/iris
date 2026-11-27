//! macOS video surface: mpv renders (render API, OpenGL) into a CAOpenGLLayer
//! hosted by an NSView that sits *below* GPUI's view inside the window's
//! content view. GPUI's Metal layer is non-opaque and clears to transparent,
//! so anything GPUI doesn't paint shows the video, and GPUI can draw on top.

use std::ffi::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::thread::JoinHandle;

use super::{VideoReset, VideoSurfaceBackend};
use anyhow::{Context as _, Result, anyhow};
use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::NSRect;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_core::player::VideoRecoveryHandle;
use iris_mpv::render::{RenderContext, RenderHandle};
use objc::declare::ClassDecl;
use objc::rc::autoreleasepool;
use objc::runtime::{BOOL, Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

type CGLPixelFormatObj = *mut c_void;
type CGLContextObj = *mut c_void;

const K_CGLPFA_DOUBLE_BUFFER: c_int = 5;
const K_CGLPFA_COLOR_SIZE: c_int = 8;
const K_CGLPFA_ALPHA_SIZE: c_int = 11;
const K_CGLPFA_ACCELERATED: c_int = 73;
const K_CGLPFA_ALLOW_OFFLINE_RENDERERS: c_int = 96;
const K_CGLPFA_OPENGL_PROFILE: c_int = 99;
const K_CGLPFA_SUPPORTS_AUTOMATIC_GRAPHICS_SWITCHING: c_int = 101;
const K_CGLOGLP_VERSION_3_2_CORE: c_int = 0x3200;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_VIEWPORT: u32 = 0x0BA2;
const RTLD_LAZY: c_int = 1;
const NS_WINDOW_BELOW: isize = -1;
const NS_VIEW_WIDTH_SIZABLE: usize = 2;
const NS_VIEW_HEIGHT_SIZABLE: usize = 16;

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLChoosePixelFormat(
        attribs: *const c_int,
        pix: *mut CGLPixelFormatObj,
        npix: *mut c_int,
    ) -> c_int;
    fn CGLCreateContext(
        pix: CGLPixelFormatObj,
        share: CGLContextObj,
        ctx: *mut CGLContextObj,
    ) -> c_int;
    fn CGLRetainPixelFormat(pix: CGLPixelFormatObj) -> CGLPixelFormatObj;
    fn CGLRetainContext(ctx: CGLContextObj) -> CGLContextObj;
    fn CGLReleaseContext(ctx: CGLContextObj);
    fn CGLReleasePixelFormat(pix: CGLPixelFormatObj);
    fn CGLGetCurrentContext() -> CGLContextObj;
    fn CGLSetCurrentContext(ctx: CGLContextObj) -> c_int;
    fn CGLLockContext(ctx: CGLContextObj) -> c_int;
    fn CGLUnlockContext(ctx: CGLContextObj) -> c_int;
    fn glGetIntegerv(pname: u32, data: *mut c_int);
    fn glFlush();
}

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceDisplayP3: *const c_void;
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn pthread_main_np() -> c_int;
}

unsafe extern "C" fn gl_get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    ffi_callback(ptr::null_mut(), || {
        static HANDLE: LazyLock<usize> = LazyLock::new(|| unsafe {
            dlopen(
                c"/System/Library/Frameworks/OpenGL.framework/OpenGL".as_ptr(),
                RTLD_LAZY,
            ) as usize
        });
        let handle = *HANDLE as *mut c_void;
        if handle.is_null() {
            ptr::null_mut()
        } else {
            unsafe { dlsym(handle, name) }
        }
    })
}

/// Shared between the layer callbacks (CA threads), the redraw thread and the UI.
/// Owns `gl` and `pixel_format` so they always outlive any active CA callback.
struct LayerState {
    pixel_format: CGLPixelFormatObj,
    gl: CGLContextObj,
    render: Mutex<Option<RenderContext>>,
    /// The update thread has seen a frame Core Animation has yet to draw.
    pending: AtomicBool,
    /// Render even without a new mpv frame (resize, margins changed).
    force: AtomicBool,
    drawable: AtomicBool,
}

unsafe impl Send for LayerState {}
unsafe impl Sync for LayerState {}

// Used outside CA's draw callback, which already owns the CGL lock and has
// the context current. Always acquire CGL before the render mutex.
struct CurrentContext {
    gl: CGLContextObj,
    previous: CGLContextObj,
}

impl CurrentContext {
    fn lock(gl: CGLContextObj) -> Option<Self> {
        unsafe {
            if CGLLockContext(gl) != 0 {
                iris_core::logging::event_kv(
                    iris_core::logging::EventLevel::Error,
                    "video",
                    "cgl_lock_failed",
                    &[("backend", "cgl")],
                );
                return None;
            }
            let previous = CGLGetCurrentContext();
            if CGLSetCurrentContext(gl) != 0 {
                CGLUnlockContext(gl);
                iris_core::logging::event_kv(
                    iris_core::logging::EventLevel::Error,
                    "video",
                    "cgl_context_failed",
                    &[("backend", "cgl")],
                );
                return None;
            }
            Some(Self { gl, previous })
        }
    }
}

impl Drop for CurrentContext {
    fn drop(&mut self) {
        unsafe {
            CGLSetCurrentContext(self.previous);
            CGLUnlockContext(self.gl);
        }
    }
}

impl LayerState {
    fn needs_display(&self) -> bool {
        self.drawable.load(Ordering::Acquire)
            && (self.force.load(Ordering::Acquire) || self.pending.load(Ordering::Acquire))
    }

    fn service_update(&self) {
        let Some(_gl) = CurrentContext::lock(self.gl) else {
            return;
        };
        let guard = self.render.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `_gl` locks the matching CGL context and keeps it current.
        if let Some(render) = guard.as_ref()
            && unsafe { render.update() } & RenderContext::UPDATE_FRAME != 0
        {
            self.pending.store(true, Ordering::Release);
        }
    }

    fn skip_pending_frame(&self) -> bool {
        if !self.pending.load(Ordering::Acquire) {
            return false;
        }
        let Some(_gl) = CurrentContext::lock(self.gl) else {
            return false;
        };
        let guard = self.render.lock().unwrap_or_else(|e| e.into_inner());
        let Some(render) = guard.as_ref() else {
            return false;
        };
        // CA may have drawn on another thread while we waited for the lock.
        if !self.pending.load(Ordering::Acquire) {
            return false;
        }
        if let Err(e) = unsafe { render.skip_frame() } {
            let err = e.to_string();
            iris_core::logging::event_kv(
                iris_core::logging::EventLevel::Error,
                "video",
                "skip_frame_error",
                &[("backend", "cgl"), ("error", &err)],
            );
            return false;
        }
        render.report_swap();
        self.pending.store(false, Ordering::Release);
        // mpv retains the skipped image as its current frame for the next draw.
        self.force.store(true, Ordering::Release);
        true
    }
}

impl Drop for LayerState {
    fn drop(&mut self) {
        // Ensure render context is freed before releasing the GL context.
        if let Some(render) = self
            .render
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            unsafe {
                CGLLockContext(self.gl);
                let previous = CGLGetCurrentContext();
                CGLSetCurrentContext(self.gl);
                drop(render);
                CGLSetCurrentContext(previous);
                CGLUnlockContext(self.gl);
            }
        }
        unsafe {
            if !self.gl.is_null() {
                CGLReleaseContext(self.gl);
            }
            if !self.pixel_format.is_null() {
                CGLReleasePixelFormat(self.pixel_format);
            }
        }
    }
}

struct SendId(id);
unsafe impl Send for SendId {}

pub(super) struct MacVideoSurface {
    state: Arc<LayerState>,
    view: id,
    layer: id,
    redraw_tx: Option<SyncSender<()>>,
    worker: Option<JoinHandle<()>>,
    scale: f64,
}

fn state_of(this: &Object) -> Option<&LayerState> {
    unsafe {
        let p: *mut c_void = *this.get_ivar("irisState");
        (p as *const LayerState).as_ref()
    }
}

// Callback fallbacks must not drop arbitrary panic payloads on the FFI stack.
fn ffi_callback<T>(fallback: T, callback: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(callback)).unwrap_or_else(|payload| {
        std::mem::forget(payload);
        fallback
    })
}

extern "C" fn layer_init_with_layer(this: &mut Object, _: Sel, other: id) -> id {
    ffi_callback(nil, || unsafe {
        let initialized: id = msg_send![super(this, class!(CAOpenGLLayer)), initWithLayer: other];
        if initialized != nil && other != nil {
            let cls = layer_class();
            let is_kind: BOOL = msg_send![other, isKindOfClass: cls];
            if is_kind == YES {
                let state: *mut c_void = *(*other).get_ivar("irisState");
                if !state.is_null() {
                    Arc::increment_strong_count(state.cast::<LayerState>());
                    (*initialized).set_ivar("irisState", state);
                }
            }
        }
        initialized
    })
}

extern "C" fn layer_dealloc(this: &Object, _: Sel) {
    ffi_callback((), || unsafe {
        let state: *mut c_void = *this.get_ivar("irisState");
        if !state.is_null() {
            drop(Arc::from_raw(state.cast::<LayerState>()));
        }
    });
    unsafe {
        let _: () = msg_send![super(this, class!(CAOpenGLLayer)), dealloc];
    }
}

extern "C" fn copy_pixel_format(this: &Object, _: Sel, _mask: u32) -> CGLPixelFormatObj {
    ffi_callback(ptr::null_mut(), || {
        state_of(this).map_or(ptr::null_mut(), |s| unsafe {
            CGLRetainPixelFormat(s.pixel_format)
        })
    })
}

extern "C" fn copy_context(this: &Object, _: Sel, _pf: CGLPixelFormatObj) -> CGLContextObj {
    ffi_callback(ptr::null_mut(), || {
        state_of(this).map_or(ptr::null_mut(), |s| unsafe { CGLRetainContext(s.gl) })
    })
}

extern "C" fn can_draw(
    this: &Object,
    _: Sel,
    _ctx: CGLContextObj,
    _pf: CGLPixelFormatObj,
    _t: f64,
    _ts: *const c_void,
) -> BOOL {
    ffi_callback(NO, || {
        let Some(state) = state_of(this) else {
            return NO;
        };
        if state.needs_display() { YES } else { NO }
    })
}

extern "C" fn draw(
    this: &Object,
    _: Sel,
    _ctx: CGLContextObj,
    _pf: CGLPixelFormatObj,
    _t: f64,
    _ts: *const c_void,
) {
    ffi_callback((), || {
        let Some(state) = state_of(this) else { return };
        let guard = state.render.lock().unwrap_or_else(|e| e.into_inner());
        let Some(render) = guard.as_ref() else { return };
        let (mut fbo, mut vp) = (0 as c_int, [0 as c_int; 4]);
        unsafe {
            glGetIntegerv(GL_FRAMEBUFFER_BINDING, &mut fbo);
            glGetIntegerv(GL_VIEWPORT, vp.as_mut_ptr());
        }
        if vp[2] <= 0 || vp[3] <= 0 || !state.drawable.load(Ordering::Acquire) {
            state.force.store(true, Ordering::Release);
            return;
        }
        // CA has made its FBO and the context supplied by copy_context current.
        // Never block the main thread waiting for a frame's display time.
        state.force.swap(false, Ordering::AcqRel);
        let block = unsafe { pthread_main_np() } == 0;
        if let Err(e) = unsafe { render.render_gl(fbo, vp[2], vp[3], true, block) } {
            state.force.store(true, Ordering::Release);
            let err = e.to_string();
            iris_core::logging::event_kv(
                iris_core::logging::EventLevel::Error,
                "video",
                "frame_error",
                &[("backend", "cgl"), ("error", &err)],
            );
            return;
        }
        unsafe { glFlush() };
        state.pending.store(false, Ordering::Release);
        render.report_swap();
    });
}

fn layer_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl =
            ClassDecl::new("IrisVideoLayer", class!(CAOpenGLLayer)).expect("IrisVideoLayer class");
        decl.add_ivar::<*mut c_void>("irisState");
        unsafe {
            decl.add_method(
                sel!(initWithLayer:),
                layer_init_with_layer as extern "C" fn(&mut Object, Sel, id) -> id,
            );
            decl.add_method(sel!(dealloc), layer_dealloc as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(copyCGLPixelFormatForDisplayMask:),
                copy_pixel_format as extern "C" fn(&Object, Sel, u32) -> CGLPixelFormatObj,
            );
            decl.add_method(
                sel!(copyCGLContextForPixelFormat:),
                copy_context as extern "C" fn(&Object, Sel, CGLPixelFormatObj) -> CGLContextObj,
            );
            decl.add_method(
                sel!(canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:),
                can_draw
                    as extern "C" fn(
                        &Object,
                        Sel,
                        CGLContextObj,
                        CGLPixelFormatObj,
                        f64,
                        *const c_void,
                    ) -> BOOL,
            );
            decl.add_method(
                sel!(drawInCGLContext:pixelFormat:forLayerTime:displayTime:),
                draw as extern "C" fn(
                    &Object,
                    Sel,
                    CGLContextObj,
                    CGLPixelFormatObj,
                    f64,
                    *const c_void,
                ),
            );
        }
        decl.register();
    });
    Class::get("IrisVideoLayer").expect("IrisVideoLayer registered")
}

impl VideoSurfaceBackend for MacVideoSurface {
    /// Creates the GL context and mpv render context, and inserts the video
    /// view under GPUI's view. Must run on the main thread.
    fn attach(
        window: &gpui::Window,
        mpv: RenderHandle,
        _recovery: VideoRecoveryHandle,
    ) -> Result<(Self, UnboundedReceiver<VideoReset>)> {
        let gpui_view: id = match HasWindowHandle::window_handle(window)
            .map_err(|e| anyhow!("{e:?}"))?
            .as_raw()
        {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as id,
            other => return Err(anyhow!("unexpected window handle {other:?}")),
        };

        unsafe {
            // GL context for mpv. CAOpenGLLayer is handed this same context.
            let attrs = [
                K_CGLPFA_OPENGL_PROFILE,
                K_CGLOGLP_VERSION_3_2_CORE,
                K_CGLPFA_DOUBLE_BUFFER,
                K_CGLPFA_ACCELERATED,
                K_CGLPFA_ALLOW_OFFLINE_RENDERERS,
                K_CGLPFA_SUPPORTS_AUTOMATIC_GRAPHICS_SWITCHING,
                K_CGLPFA_COLOR_SIZE,
                24,
                K_CGLPFA_ALPHA_SIZE,
                8,
                0,
            ];
            let (mut pf, mut npix) = (ptr::null_mut(), 0);
            if CGLChoosePixelFormat(attrs.as_ptr(), &mut pf, &mut npix) != 0 || pf.is_null() {
                return Err(anyhow!("CGLChoosePixelFormat failed"));
            }
            let mut gl = ptr::null_mut();
            if CGLCreateContext(pf, ptr::null_mut(), &mut gl) != 0 || gl.is_null() {
                CGLReleasePixelFormat(pf);
                return Err(anyhow!("CGLCreateContext failed"));
            }
            // Wrap GL resources in LayerState immediately so any early `?` frees them.
            let state = Arc::new(LayerState {
                pixel_format: pf,
                gl,
                render: Mutex::new(None),
                pending: AtomicBool::new(false),
                force: AtomicBool::new(true),
                drawable: AtomicBool::new(false),
            });

            // The layer is tagged Display P3 (below), so mpv must output
            // Display P3 with the sRGB curve. Core Animation then converts to
            // the screen, as it does for GPUI's layer.
            mpv.set_display_color_target("display-p3", "srgb")?;

            let mut render = {
                let _current = CurrentContext::lock(gl)
                    .ok_or_else(|| anyhow!("could not activate the video CGL context"))?;
                RenderContext::new_gl(mpv, gl_get_proc_address, ptr::null_mut())
                    .context("creating mpv render context")?
            };

            // The layer owns its state from publication through Objective-C
            // deallocation. Core Animation can retain it beyond view removal.
            let layer: id = msg_send![layer_class(), new];
            (*layer).set_ivar("irisState", Arc::into_raw(state.clone()) as *mut c_void);
            let _: id = msg_send![layer, retain];
            let layer_ref = SendId(layer);
            let worker_state = Arc::clone(&state);
            let (tx, rx) = mpsc::sync_channel::<()>(1);
            let worker =
                iris_base::rt::spawn_thread(iris_base::rt::ThreadRole::VideoSurface, move || {
                    let layer = layer_ref;
                    while rx.recv().is_ok() {
                        // CA can decline to call can_draw/draw (minimized, hidden,
                        // or on another Space). Always service mpv independently.
                        worker_state.service_update();
                        if worker_state.needs_display() {
                            autoreleasepool(|| {
                                let _: () = msg_send![class!(CATransaction), begin];
                                let _: () = msg_send![layer.0, display];
                                let _: () = msg_send![class!(CATransaction), commit];
                                let _: () = msg_send![class!(CATransaction), flush];
                            });
                        }
                        if worker_state.skip_pending_frame() {
                            autoreleasepool(|| {
                                let _: () = msg_send![class!(CATransaction), begin];
                                let _: () = msg_send![layer.0, setNeedsDisplay];
                                let _: () = msg_send![class!(CATransaction), commit];
                            });
                        }
                    }
                    autoreleasepool(|| {
                        let _: () = msg_send![layer.0, release];
                    });
                });
            let worker = match worker {
                Ok(worker) => worker,
                Err(error) => {
                    {
                        let _current = CurrentContext::lock(gl);
                        drop(render);
                    }
                    let _: () = msg_send![layer, release];
                    let _: () = msg_send![layer, release];
                    return Err(error.into());
                }
            };

            let _: () = msg_send![layer, setAsynchronous: NO];
            let _: () = msg_send![layer, setOpaque: YES];
            let _: () = msg_send![layer, setNeedsDisplayOnBoundsChange: YES];
            let p3 = CGColorSpaceCreateWithName(kCGColorSpaceDisplayP3);
            if !p3.is_null() {
                let _: () = msg_send![layer, setColorspace: p3];
                CGColorSpaceRelease(p3);
            }
            let ns_window: id = msg_send![gpui_view, window];
            let raw_scale: f64 = msg_send![ns_window, backingScaleFactor];
            let scale = if raw_scale.is_finite() && raw_scale > 0. {
                raw_scale
            } else {
                1.
            };
            let _: () = msg_send![layer, setContentsScale: scale];
            let content: id = msg_send![ns_window, contentView];
            let frame: NSRect = msg_send![content, bounds];
            let view: id = msg_send![class!(NSView), alloc];
            let view: id = msg_send![view, initWithFrame: frame];
            let _: () = msg_send![view, setLayer: layer];
            let _: () = msg_send![view, setWantsLayer: YES];
            let _: () = msg_send![view, setAutoresizingMask: NS_VIEW_WIDTH_SIZABLE | NS_VIEW_HEIGHT_SIZABLE];
            let _: () = msg_send![content, addSubview: view positioned: NS_WINDOW_BELOW relativeTo: gpui_view];

            state.drawable.store(
                frame.origin.x.is_finite()
                    && frame.origin.y.is_finite()
                    && frame.size.width.is_finite()
                    && frame.size.height.is_finite()
                    && frame.size.width > 0.
                    && frame.size.height > 0.,
                Ordering::Release,
            );
            let cb_tx = tx.clone();
            render.set_update_callback(move || {
                let _ = cb_tx.try_send(());
            });
            *state.render.lock().unwrap_or_else(|e| e.into_inner()) = Some(render);
            // Registration may wake the worker before render is installed.
            let _ = tx.try_send(());

            let _: () = msg_send![layer, setNeedsDisplay];
            Ok((
                Self {
                    state,
                    view,
                    layer,
                    redraw_tx: Some(tx),
                    worker: Some(worker),
                    scale,
                },
                unbounded().1,
            ))
        }
    }

    /// Re-renders the current frame even if mpv has no new one.
    fn request_redraw(&self) {
        self.state.force.store(true, Ordering::Release);
        if let Some(tx) = &self.redraw_tx {
            let _ = tx.try_send(());
        }
    }

    /// Places the video view over the video area, given in GPUI window
    /// coordinates (logical points, top-left origin). mpv's OSD and subtitles
    /// then live inside this rect, letterbox bars included.
    fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        // Publish visibility without reading mutable layer geometry on CA threads.
        self.state.drawable.store(false, Ordering::Release);
        if !x.is_finite()
            || !y.is_finite()
            || !w.is_finite()
            || !h.is_finite()
            || w <= 0.
            || h <= 0.
        {
            unsafe {
                let _: () = msg_send![self.view, setHidden: YES];
            }
            self.request_redraw();
            return;
        }
        unsafe {
            let superview: id = msg_send![self.view, superview];
            if superview == nil {
                return;
            }
            let bounds: NSRect = msg_send![superview, bounds];
            let flipped: BOOL = msg_send![superview, isFlipped];
            let ay = if flipped == YES {
                y
            } else {
                bounds.size.height - (y + h)
            };
            if !ay.is_finite() {
                let _: () = msg_send![self.view, setHidden: YES];
                self.request_redraw();
                return;
            }
            let frame = NSRect::new(
                cocoa::foundation::NSPoint::new(x, ay),
                cocoa::foundation::NSSize::new(w, h),
            );
            // No implicit animation: the frame must jump with GPUI's layout.
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
            let _: () = msg_send![self.view, setFrame: frame];
            let _: () = msg_send![self.view, setHidden: NO];
            let _: () = msg_send![class!(CATransaction), commit];
        }
        self.state.drawable.store(true, Ordering::Release);
        self.request_redraw();
    }

    /// Keeps the layer at the window's backing scale (Retina vs. non-Retina screens).
    fn set_scale(&mut self, scale: f64) {
        if !scale.is_finite() || scale <= 0. {
            return;
        }
        if (scale - self.scale).abs() > f64::EPSILON {
            self.scale = scale;
            unsafe {
                let _: () = msg_send![self.layer, setContentsScale: scale];
            }
            self.request_redraw();
        }
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn debug_force_device_reset(&self) -> bool {
        false
    }
}

impl MacVideoSurface {
    /// Stops mpv callbacks, joins the video thread, and detaches its view.
    fn shutdown(&mut self) {
        // 1. Free the render context first so mpv's update callback stops firing
        //    and releases its clone of `redraw_tx`.
        self.state.drawable.store(false, Ordering::Release);
        let render = self
            .state
            .render
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(render) = render {
            unsafe {
                CGLLockContext(self.state.gl);
                let previous = CGLGetCurrentContext();
                CGLSetCurrentContext(self.state.gl);
                drop(render);
                CGLSetCurrentContext(previous);
                CGLUnlockContext(self.state.gl);
            }
        }
        // 2. Drop our sender and wait for the render thread before detaching.
        self.redraw_tx.take();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("video worker panicked");
        }
        unsafe {
            if self.view != nil {
                let _: () = msg_send![self.view, removeFromSuperview];
            }
        }
    }
}

impl Drop for MacVideoSurface {
    fn drop(&mut self) {
        self.shutdown();
        unsafe {
            if self.view != nil {
                let _: () = msg_send![self.view, release];
                self.view = nil;
            }
            if self.layer != nil {
                let _: () = msg_send![self.layer, release];
                self.layer = nil;
            }
        }
    }
}
