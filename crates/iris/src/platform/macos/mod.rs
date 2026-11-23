//! macOS native window, sleep prevention, trackpad pinch, media key,
//! and debug synthetic mouse helpers.

mod imageio;

use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect, NSString};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{MediaKey, NowPlaying, PinchEvent};

pub(super) use imageio::decode_thumbnail_imageio;
#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) use imageio::snapshot_window;

pub(super) fn ns_view(window: &gpui::Window) -> Option<id> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    Some(h.ns_view.as_ptr() as id)
}

fn ns_window(window: &gpui::Window) -> Option<id> {
    let view = ns_view(window)?;
    let win: id = unsafe { msg_send![view, window] };
    (win != nil).then_some(win)
}

/// Hides the system close/minimise/zoom buttons; Iris draws its own.
pub(crate) fn hide_traffic_lights(window: &gpui::Window) {
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

unsafe extern "C" {
    fn objc_getAssociatedObject(object: id, key: *const c_void) -> id;
    fn objc_setAssociatedObject(object: id, key: *const c_void, value: id, policy: usize);
}

fn floating_collection_behavior(original: usize) -> usize {
    const JOIN_ALL_SPACES: usize = 1 << 0;
    const MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;
    const FULL_SCREEN_PRIMARY: usize = 1 << 7;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    const FULL_SCREEN_NONE: usize = 1 << 9;
    (original & !(MOVE_TO_ACTIVE_SPACE | FULL_SCREEN_PRIMARY | FULL_SCREEN_NONE))
        | JOIN_ALL_SPACES
        | FULL_SCREEN_AUXILIARY
}

/// Keep the original level and collection behavior on the NSWindow itself so
/// repeated preset applications cannot overwrite them or outlive the window.
pub(crate) fn set_always_on_top(window: &gpui::Window, on: bool) {
    static RESTORE_KEY: u8 = 0;
    const RETAIN_NONATOMIC: usize = 1;
    const FLOATING_LEVEL: isize = 3;
    let Some(win) = ns_window(window) else { return };
    let key = (&raw const RESTORE_KEY).cast();
    unsafe {
        let saved = objc_getAssociatedObject(win, key);
        if on && saved == nil {
            let level: isize = msg_send![win, level];
            let behavior: usize = msg_send![win, collectionBehavior];
            let values: [id; 2] = [
                msg_send![class!(NSNumber), numberWithInteger: level],
                msg_send![class!(NSNumber), numberWithUnsignedInteger: behavior],
            ];
            let snapshot: id =
                msg_send![class!(NSArray), arrayWithObjects: values.as_ptr() count: values.len()];
            objc_setAssociatedObject(win, key, snapshot, RETAIN_NONATOMIC);
            let _: () =
                msg_send![win, setCollectionBehavior: floating_collection_behavior(behavior)];
            let _: () = msg_send![win, setLevel: level.max(FLOATING_LEVEL)];
        } else if !on && saved != nil {
            let saved_level: id = msg_send![saved, objectAtIndex: 0usize];
            let saved_behavior: id = msg_send![saved, objectAtIndex: 1usize];
            let level: isize = msg_send![saved_level, integerValue];
            let behavior: usize = msg_send![saved_behavior, unsignedIntegerValue];
            let _: () = msg_send![win, setLevel: level];
            let _: () = msg_send![win, setCollectionBehavior: behavior];
            objc_setAssociatedObject(win, key, nil, RETAIN_NONATOMIC);
        }
    }
}

// ---- idle display sleep prevention (IOKit) ----

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,
        assertion_level: u32,
        assertion_name: *const c_void,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(super) fn CFStringCreateWithCString(
        alloc: *const c_void,
        s: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    pub(super) fn CFURLCreateFromFileSystemRepresentation(
        alloc: *const c_void,
        buf: *const u8,
        len: isize,
        is_dir: bool,
    ) -> *const c_void;
    pub(super) fn CFNumberCreate(
        alloc: *const c_void,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> *const c_void;
    pub(super) fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_cb: *const c_void,
        val_cb: *const c_void,
    ) -> *const c_void;
    pub(super) fn CFRelease(cf: *const c_void);
    pub(super) static kCFBooleanTrue: *const c_void;
    pub(super) static kCFTypeDictionaryKeyCallBacks: c_void;
    pub(super) static kCFTypeDictionaryValueCallBacks: c_void;
}

#[derive(Default)]
pub(crate) struct SleepPreventer {
    assertion_id: Option<u32>,
}

impl SleepPreventer {
    pub(crate) fn new() -> Self {
        Self { assertion_id: None }
    }

    pub(crate) fn set_active(&mut self, active: bool) {
        if active && self.assertion_id.is_none() {
            unsafe {
                let cf_ty = CFStringCreateWithCString(
                    ptr::null(),
                    c"NoDisplaySleepAssertion".as_ptr(),
                    0x0800_0100,
                );
                let cf_reason = CFStringCreateWithCString(
                    ptr::null(),
                    c"Iris video playback".as_ptr(),
                    0x0800_0100,
                );
                let mut id_out: u32 = 0;
                // kIOPMAssertionLevelOn = 255.
                if IOPMAssertionCreateWithName(cf_ty, 255, cf_reason, &mut id_out) == 0 {
                    self.assertion_id = Some(id_out);
                }
                CFRelease(cf_reason);
                CFRelease(cf_ty);
            }
        } else if !active && let Some(id_val) = self.assertion_id.take() {
            unsafe {
                let _ = IOPMAssertionRelease(id_val);
            }
        }
    }
}

impl Drop for SleepPreventer {
    fn drop(&mut self) {
        self.set_active(false);
    }
}

// ---- native trackpad pinch (NSMagnificationGestureRecognizer) ----

struct PinchTargetState {
    tx: UnboundedSender<PinchEvent>,
    view: id,
}

extern "C" fn on_magnify(this: &Object, _: Sel, recognizer: id) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| unsafe {
        let p: *mut c_void = *this.get_ivar("irisPinch");
        if p.is_null() {
            return;
        }
        let state = &*(p as *const PinchTargetState);
        let mag: f64 = msg_send![recognizer, magnification];
        let _: () = msg_send![recognizer, setMagnification: 0.0f64];
        if !mag.is_finite() || mag.abs() < 1e-4 {
            return;
        }
        let loc: NSPoint = msg_send![recognizer, locationInView: state.view];
        let bounds: NSRect = msg_send![state.view, bounds];
        let flipped: cocoa::base::BOOL = msg_send![state.view, isFlipped];
        let y = if flipped == YES {
            loc.y
        } else {
            bounds.size.height - loc.y
        };
        let _ = state.tx.unbounded_send(PinchEvent {
            delta: mag * 1.6,
            position: [loc.x, y],
        });
    })) {
        std::mem::forget(payload);
    }
}

fn pinch_target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl =
            ClassDecl::new("IrisPinchTarget", class!(NSObject)).expect("IrisPinchTarget class");
        decl.add_ivar::<*mut c_void>("irisPinch");
        unsafe {
            decl.add_method(
                sel!(onMagnify:),
                on_magnify as extern "C" fn(&Object, Sel, id),
            );
        }
        decl.register();
    });
    Class::get("IrisPinchTarget").expect("IrisPinchTarget")
}

pub(crate) struct PinchMonitor {
    view: id,
    recognizer: id,
    target: id,
    state: *mut PinchTargetState,
}

impl PinchMonitor {
    pub(crate) fn attach(window: &gpui::Window) -> Option<(Self, UnboundedReceiver<PinchEvent>)> {
        let view = ns_view(window)?;
        let (tx, rx) = unbounded();
        let state = Box::into_raw(Box::new(PinchTargetState { tx, view }));
        unsafe {
            // Retain the NSView so Drop never messages a deallocated view during window close.
            let _: id = msg_send![view, retain];
            let target: id = msg_send![pinch_target_class(), new];
            (*target).set_ivar("irisPinch", state as *mut c_void);
            let rec: id = msg_send![class!(NSMagnificationGestureRecognizer), alloc];
            let rec: id = msg_send![rec, initWithTarget: target action: sel!(onMagnify:)];
            let _: () = msg_send![view, addGestureRecognizer: rec];
            Some((
                Self {
                    view,
                    recognizer: rec,
                    target,
                    state,
                },
                rx,
            ))
        }
    }
}

impl Drop for PinchMonitor {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.view, removeGestureRecognizer: self.recognizer];
            (*self.target).set_ivar("irisPinch", ptr::null_mut::<c_void>());
            let _: () = msg_send![self.recognizer, release];
            let _: () = msg_send![self.target, release];
            let _: () = msg_send![self.view, release];
            drop(Box::from_raw(self.state));
        }
    }
}

// ---- macOS system media keys & Now Playing (MediaPlayer.framework) ----

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

struct MediaTargetState {
    tx: UnboundedSender<MediaKey>,
    enabled: AtomicBool,
}

fn send_media_key(this: &Object, key: MediaKey) -> isize {
    const COMMAND_FAILED: isize = 200;
    catch_unwind(AssertUnwindSafe(|| unsafe {
        let p: *mut c_void = *this.get_ivar("irisMedia");
        let Some(state) = p.cast::<MediaTargetState>().as_ref() else {
            return COMMAND_FAILED;
        };
        if state.enabled.load(Ordering::Acquire) && state.tx.unbounded_send(key).is_ok() {
            0 // MPRemoteCommandHandlerStatusSuccess
        } else {
            COMMAND_FAILED
        }
    }))
    .unwrap_or_else(|payload| {
        std::mem::forget(payload);
        COMMAND_FAILED
    })
}

extern "C" fn cmd_play(this: &Object, _: Sel, _: id) -> isize {
    send_media_key(this, MediaKey::Play)
}
extern "C" fn cmd_pause(this: &Object, _: Sel, _: id) -> isize {
    send_media_key(this, MediaKey::Pause)
}
extern "C" fn cmd_toggle(this: &Object, _: Sel, _: id) -> isize {
    send_media_key(this, MediaKey::Toggle)
}
extern "C" fn cmd_next(this: &Object, _: Sel, _: id) -> isize {
    send_media_key(this, MediaKey::Next)
}
extern "C" fn cmd_prev(this: &Object, _: Sel, _: id) -> isize {
    send_media_key(this, MediaKey::Prev)
}

extern "C" fn media_target_dealloc(this: &Object, _: Sel) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| unsafe {
        let state: *mut c_void = *this.get_ivar("irisMedia");
        if !state.is_null() {
            drop(Box::from_raw(state.cast::<MediaTargetState>()));
        }
    })) {
        std::mem::forget(payload);
    }
    unsafe {
        let _: () = msg_send![super(this, class!(NSObject)), dealloc];
    }
}

fn media_commands() -> [(Sel, Sel); 5] {
    [
        (sel!(playCommand), sel!(onPlay:)),
        (sel!(pauseCommand), sel!(onPause:)),
        (sel!(togglePlayPauseCommand), sel!(onToggle:)),
        (sel!(nextTrackCommand), sel!(onNext:)),
        (sel!(previousTrackCommand), sel!(onPrev:)),
    ]
}

fn media_target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let mut decl = ClassDecl::new("IrisMediaKeyTarget", class!(NSObject))
            .expect("IrisMediaKeyTarget class");
        decl.add_ivar::<*mut c_void>("irisMedia");
        unsafe {
            decl.add_method(
                sel!(dealloc),
                media_target_dealloc as extern "C" fn(&Object, Sel),
            );
            for (s, f) in [
                (
                    sel!(onPlay:),
                    cmd_play as extern "C" fn(&Object, Sel, id) -> isize,
                ),
                (sel!(onPause:), cmd_pause),
                (sel!(onToggle:), cmd_toggle),
                (sel!(onNext:), cmd_next),
                (sel!(onPrev:), cmd_prev),
            ] {
                decl.add_method(s, f);
            }
        }
        decl.register();
    });
    Class::get("IrisMediaKeyTarget").expect("IrisMediaKeyTarget")
}

pub(crate) struct MediaKeyBridge {
    target: id,
    state: *mut MediaTargetState,
    command_center: id,
}

impl MediaKeyBridge {
    pub(crate) fn attach(_window: &gpui::Window) -> Option<(Self, UnboundedReceiver<MediaKey>)> {
        unsafe {
            if dlopen(
                c"/System/Library/Frameworks/MediaPlayer.framework/MediaPlayer".as_ptr(),
                1,
            )
            .is_null()
            {
                return None;
            }
            let center_cls = Class::get("MPRemoteCommandCenter")?;
            let center: id = msg_send![center_cls, sharedCommandCenter];
            if center == nil {
                return None;
            }
            let (tx, rx) = unbounded();
            let state = Box::into_raw(Box::new(MediaTargetState {
                tx,
                enabled: AtomicBool::new(false),
            }));
            let target: id = msg_send![media_target_class(), new];
            (*target).set_ivar("irisMedia", state as *mut c_void);

            for (cmd_sel, action) in media_commands() {
                let cmd: id = msg_send![center, performSelector: cmd_sel];
                if cmd != nil {
                    let _: () = msg_send![cmd, setEnabled: NO];
                    let _: () = msg_send![cmd, addTarget: target action: action];
                }
            }

            Some((
                Self {
                    target,
                    state,
                    command_center: center,
                },
                rx,
            ))
        }
    }

    fn set_commands_enabled(&self, enabled: bool) {
        // The target owns state. This bridge retains the target until Drop.
        if unsafe { (*self.state).enabled.swap(enabled, Ordering::AcqRel) } == enabled {
            return;
        }
        let enabled = if enabled { YES } else { NO };
        unsafe {
            for (selector, _) in media_commands() {
                let cmd: id = msg_send![self.command_center, performSelector: selector];
                let _: () = msg_send![cmd, setEnabled: enabled];
            }
        }
    }

    pub(crate) fn update_now_playing(&mut self, media: Option<NowPlaying<'_>>) {
        self.set_commands_enabled(media.is_some());
        unsafe {
            let Some(info_cls) = Class::get("MPNowPlayingInfoCenter") else {
                return;
            };
            let center: id = msg_send![info_cls, defaultCenter];
            if center == nil {
                return;
            }
            let Some(media) = media else {
                let _: () = msg_send![center, setNowPlayingInfo: nil];
                let _: () = msg_send![center, setPlaybackState: 0usize];
                return;
            };
            let dict: id = msg_send![class!(NSMutableDictionary), dictionary];
            let ns_cstr = |ptr: *const c_char| -> id {
                msg_send![class!(NSString), stringWithUTF8String: ptr]
            };
            let ns_num = |v: f64| -> id { msg_send![class!(NSNumber), numberWithDouble: v] };
            let state = media.state;
            let title = NSString::alloc(nil).init_str(&media.title);
            let _: () = msg_send![dict, setObject: title forKey: ns_cstr(c"title".as_ptr())];
            let _: () = msg_send![title, release];
            let media_type = if media.is_video { 2. } else { 1. };
            let _: () = msg_send![dict, setObject: ns_num(media_type) forKey: ns_cstr(c"MPNowPlayingInfoPropertyMediaType".as_ptr())];
            if state.duration > 0. {
                let _: () = msg_send![dict, setObject: ns_num(state.duration) forKey: ns_cstr(c"playbackDuration".as_ptr())];
                let _: () = msg_send![dict, setObject: ns_num(state.time_pos.max(0.)) forKey: ns_cstr(c"MPNowPlayingInfoPropertyElapsedPlaybackTime".as_ptr())];
            }
            let rate = if state.paused || state.ended {
                0.
            } else {
                state.speed
            };
            let _: () = msg_send![dict, setObject: ns_num(rate) forKey: ns_cstr(c"MPNowPlayingInfoPropertyPlaybackRate".as_ptr())];
            let _: () = msg_send![center, setNowPlayingInfo: dict];
            let pb_state: usize = if state.ended {
                3
            } else if state.paused {
                2
            } else {
                1
            };
            let _: () = msg_send![center, setPlaybackState: pb_state];
        }
    }
}

impl Drop for MediaKeyBridge {
    fn drop(&mut self) {
        self.update_now_playing(None);
        unsafe {
            for (selector, _) in media_commands() {
                let cmd: id = msg_send![self.command_center, performSelector: selector];
                let _: () = msg_send![cmd, removeTarget: self.target];
            }
            let _: () = msg_send![self.target, release];
        }
    }
}

/// Starts a native window drag from the current mouse-down / mouse-dragged event.
pub(crate) fn begin_window_drag(window: &gpui::Window) {
    let Some(win) = ns_window(window) else { return };
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let event: id = msg_send![app, currentEvent];
        if event != nil {
            let event_type: usize = msg_send![event, type];
            let source_window: id = msg_send![event, window];
            if matches!(event_type, 1 | 6) && source_window == win {
                let _: () = msg_send![win, performWindowDragWithEvent: event];
            }
        }
    }
}

pub(crate) fn toggle_maximize(window: &gpui::Window) {
    window.zoom_window();
}

pub(crate) fn titlebar_double_click(window: &gpui::Window) {
    window.titlebar_double_click();
}

/// Debug tooling: feeds GPUI's view a synthetic mouse move (and optional left click)
/// at `(x, y)` in GPUI's top-left window points.
#[cfg(any(debug_assertions, feature = "devtools"))]
pub(crate) fn synth_mouse(
    window: &gpui::Window,
    click: bool,
    x: f64,
    y: f64,
) -> Option<Box<dyn FnOnce()>> {
    let view = unsafe { objc::rc::StrongPtr::retain(ns_view(window)?) };
    let win = unsafe { objc::rc::StrongPtr::retain(ns_window(window)?) };
    Some(Box::new(move || unsafe {
        let (view, win) = (*view, *win);
        let current: id = msg_send![view, window];
        if current != win {
            return;
        }
        let content: id = msg_send![win, contentView];
        let frame: NSRect = msg_send![content, frame];
        let loc = NSPoint::new(x, frame.size.height - y);
        let number: isize = msg_send![win, windowNumber];
        let make = |ty: u64, clicks: isize| -> id {
            msg_send![class!(NSEvent), mouseEventWithType: ty location: loc modifierFlags: 0u64
                timestamp: 0.0f64 windowNumber: number context: nil eventNumber: 0isize
                clickCount: clicks pressure: 1.0f32]
        };
        let _: () = msg_send![view, mouseMoved: make(5, 0)];
        if click {
            let _: () = msg_send![view, mouseDown: make(1, 1)];
            let _: () = msg_send![view, mouseUp: make(2, 1)];
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::floating_collection_behavior;

    #[test]
    fn floating_preserves_unrelated_space_flags_and_excludes_conflicting_roles() {
        let stationary = 1 << 4;
        let move_to_active = 1 << 1;
        for full_screen_role in [0, 1 << 7, 1 << 8, 1 << 9] {
            let original = stationary | move_to_active | full_screen_role;
            let floating = floating_collection_behavior(original);
            assert_eq!(floating, stationary | 1 | (1 << 8));
            assert_eq!(floating_collection_behavior(floating), floating);
        }
    }
}
