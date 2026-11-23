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

use anyhow::{Context as _, Result, anyhow};
use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::NSRect;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
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
    /// Render even without a new mpv frame (resize, margins changed).
    force: AtomicBool,
    drawable: AtomicBool,
}

unsafe impl Send for LayerState {}
unsafe impl Sync for LayerState {}

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
                CGLSetCurrentContext(self.gl);
                drop(render);
                CGLSetCurrentContext(ptr::null_mut());
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

pub(crate) struct VideoSurface {
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
        if !state.drawable.load(Ordering::Acquire) {
            return NO;
        }
        let guard = state.render.lock().unwrap_or_else(|e| e.into_inner());
        let Some(render) = guard.as_ref() else {
            return NO;
        };
        let force = state.force.swap(false, Ordering::AcqRel);
        let flags = render.update();
        if force || flags & RenderContext::UPDATE_FRAME != 0 {
            YES
        } else {
            NO
        }
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
        let block = unsafe { pthread_main_np() } == 0;
        if let Err(e) = unsafe { render.render_gl(fbo, vp[2], vp[3], true, block) } {
            state.force.store(true, Ordering::Release);
            log::error!("render failed: {e}");
            return;
        }
        unsafe { glFlush() };
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

impl VideoSurface {
    /// Creates the GL context and mpv render context, and inserts the video
    /// view under GPUI's view. Must run on the main thread.
    pub(crate) fn attach(
        window: &gpui::Window,
        mpv: RenderHandle,
    ) -> Result<(Self, UnboundedReceiver<()>)> {
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
                force: AtomicBool::new(true),
                drawable: AtomicBool::new(false),
            });

            // The layer is tagged Display P3 (below), so mpv must output
            // Display P3 with the sRGB curve. Core Animation then converts to
            // the screen, as it does for GPUI's layer.
            mpv.set_display_color_target("display-p3", "srgb")?;

            CGLSetCurrentContext(gl);
            let render_res = RenderContext::new_gl(mpv, gl_get_proc_address, ptr::null_mut())
                .context("creating mpv render context");
            CGLSetCurrentContext(ptr::null_mut());
            let render = render_res?;

            // The layer owns its state from publication through Objective-C
            // deallocation. Core Animation can retain it beyond view removal.
            let layer: id = msg_send![layer_class(), new];
            (*layer).set_ivar("irisState", Arc::into_raw(state.clone()) as *mut c_void);
            let _: id = msg_send![layer, retain];
            let layer_ref = SendId(layer);
            let (tx, rx) = mpsc::sync_channel::<()>(1);
            let worker = std::thread::Builder::new()
                .name("iris-video".into())
                .spawn(move || {
                    let layer = layer_ref;
                    while rx.recv().is_ok() {
                        autoreleasepool(|| {
                            let _: () = msg_send![layer.0, display];
                            let _: () = msg_send![class!(CATransaction), flush];
                        });
                    }
                    let _: () = msg_send![layer.0, release];
                });
            let worker = match worker {
                Ok(worker) => worker,
                Err(error) => {
                    CGLSetCurrentContext(gl);
                    drop(render);
                    CGLSetCurrentContext(ptr::null_mut());
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
            let scale: f64 = msg_send![ns_window, backingScaleFactor];
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
                frame.size.width > 0. && frame.size.height > 0.,
                Ordering::Release,
            );
            let cb_tx = tx.clone();
            let mut render = render;
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
    pub(crate) fn request_redraw(&self) {
        self.state.force.store(true, Ordering::Release);
        if let Some(tx) = &self.redraw_tx {
            let _ = tx.try_send(());
        }
    }

    /// Places the video view over the video area, given in GPUI window
    /// coordinates (logical points, top-left origin). mpv's OSD and subtitles
    /// then live inside this rect, letterbox bars included.
    pub(crate) fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        // Publish visibility without reading mutable layer geometry on CA threads.
        self.state.drawable.store(false, Ordering::Release);
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
            let frame = NSRect::new(
                cocoa::foundation::NSPoint::new(x, ay),
                cocoa::foundation::NSSize::new(w, h),
            );
            // No implicit animation: the frame must jump with GPUI's layout.
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: YES];
            let _: () = msg_send![self.view, setFrame: frame];
            let _: () = msg_send![class!(CATransaction), commit];
        }
        self.state
            .drawable
            .store(w > 0. && h > 0., Ordering::Release);
        self.request_redraw();
    }

    /// Keeps the layer at the window's backing scale (Retina vs. non-Retina screens).
    pub(crate) fn set_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() > f64::EPSILON {
            self.scale = scale;
            unsafe {
                let _: () = msg_send![self.layer, setContentsScale: scale];
            }
            self.request_redraw();
        }
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub(crate) fn debug_force_device_reset(&self) {}

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
                CGLSetCurrentContext(self.state.gl);
                drop(render);
                CGLSetCurrentContext(ptr::null_mut());
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

impl Drop for VideoSurface {
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
