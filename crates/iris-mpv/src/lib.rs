//! Thin, hand-written bindings to libmpv (client API 2.x and render API).
//!
//! Only what Iris uses is bound. `sys` mirrors the C headers; the rest is a
//! small safe layer: [`Mpv`] for the client handle and [`render::RenderContext`]
//! for the render API.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::ptr;

pub mod render;
pub mod sys;

#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub context: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = unsafe { CStr::from_ptr(sys::mpv_error_string(self.code)) };
        write!(
            f,
            "mpv error {} ({}): {}",
            self.code,
            msg.to_string_lossy(),
            self.context
        )
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(code: c_int, context: impl FnOnce() -> String) -> Result<()> {
    if code < 0 {
        Err(Error {
            code,
            context: context(),
        })
    } else {
        Ok(())
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new(s.replace('\0', "")).unwrap())
}

/// Stack-backed C string for short NUL-free strings (avoids heap allocations on hot paths).
fn with_cstr<R>(s: &str, f: impl FnOnce(*const c_char) -> R) -> R {
    let bytes = s.as_bytes();
    if bytes.len() < 128 && !bytes.contains(&0) {
        let mut buf = [0u8; 128];
        buf[..bytes.len()].copy_from_slice(bytes);
        f(buf.as_ptr().cast())
    } else {
        let owned = cstr(s);
        f(owned.as_ptr())
    }
}

/// Value of an observed property.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Flag(bool),
    Int(i64),
    Double(f64),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Flag,
    Int,
    Double,
    Str,
}

impl Format {
    fn raw(self) -> sys::mpv_format {
        match self {
            Format::Flag => sys::MPV_FORMAT_FLAG,
            Format::Int => sys::MPV_FORMAT_INT64,
            Format::Double => sys::MPV_FORMAT_DOUBLE,
            Format::Str => sys::MPV_FORMAT_STRING,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Other(i32),
}

#[derive(Debug, Clone)]
pub enum Event {
    Shutdown,
    Log {
        prefix: String,
        level: String,
        text: String,
    },
    StartFile,
    EndFile {
        reason: EndReason,
        error: i32,
    },
    FileLoaded,
    VideoReconfig,
    AudioReconfig,
    Seek,
    PlaybackRestart,
    PropertyChange {
        id: u64,
        name: String,
        value: Value,
    },
    Other(i32),
}

/// An mpv client handle. The client API is thread-safe, so this is `Send + Sync`;
/// only [`Mpv::wait_event`] must be called from one thread at a time.
pub struct Mpv {
    handle: *mut sys::mpv_handle,
}

unsafe impl Send for Mpv {}
unsafe impl Sync for Mpv {}

impl Mpv {
    /// Creates an uninitialized handle. Set options, then call [`Mpv::initialize`].
    pub fn create() -> Result<Self> {
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            return Err(Error {
                code: sys::MPV_ERROR_NOMEM,
                context: "mpv_create".into(),
            });
        }
        Ok(Self { handle })
    }

    pub fn raw(&self) -> *mut sys::mpv_handle {
        self.handle
    }

    pub fn set_option(&self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (cstr(name), cstr(value));
        check(
            unsafe { sys::mpv_set_option_string(self.handle, n.as_ptr(), v.as_ptr()) },
            || format!("set_option {name}={value}"),
        )
    }

    pub fn initialize(&self) -> Result<()> {
        check(unsafe { sys::mpv_initialize(self.handle) }, || {
            "mpv_initialize".into()
        })
    }

    pub fn request_log_messages(&self, min_level: &str) -> Result<()> {
        let l = cstr(min_level);
        check(
            unsafe { sys::mpv_request_log_messages(self.handle, l.as_ptr()) },
            || "request_log_messages".into(),
        )
    }

    /// Runs a command synchronously, e.g. `["loadfile", path]`.
    pub fn command(&self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        check(
            unsafe { sys::mpv_command(self.handle, ptrs.as_mut_ptr()) },
            || format!("command {args:?}"),
        )
    }

    /// Runs a command without waiting for it (the reply event is ignored).
    pub fn command_async(&self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        check(
            unsafe { sys::mpv_command_async(self.handle, 0, ptrs.as_mut_ptr()) },
            || format!("command_async {args:?}"),
        )
    }

    pub fn set_property_string(&self, name: &str, value: &str) -> Result<()> {
        with_cstr(name, |n| {
            with_cstr(value, |v| {
                check(
                    unsafe { sys::mpv_set_property_string(self.handle, n, v) },
                    || format!("set_property {name}={value}"),
                )
            })
        })
    }

    /// Sets a double property asynchronously (no round trip to the core).
    pub fn set_property_f64_async(&self, name: &str, mut value: f64) -> Result<()> {
        with_cstr(name, |n| {
            check(
                unsafe {
                    sys::mpv_set_property_async(
                        self.handle,
                        0,
                        n,
                        sys::MPV_FORMAT_DOUBLE,
                        &mut value as *mut f64 as *mut c_void,
                    )
                },
                || format!("set_property_async {name}={value}"),
            )
        })
    }

    pub fn get_property_f64(&self, name: &str) -> Result<f64> {
        let mut v = 0f64;
        with_cstr(name, |n| {
            check(
                unsafe {
                    sys::mpv_get_property(
                        self.handle,
                        n,
                        sys::MPV_FORMAT_DOUBLE,
                        &mut v as *mut f64 as *mut c_void,
                    )
                },
                || format!("get_property {name}"),
            )
        })?;
        Ok(v)
    }

    pub fn get_property_string(&self, name: &str) -> Result<String> {
        let p = with_cstr(name, |n| unsafe {
            sys::mpv_get_property_string(self.handle, n)
        });
        if p.is_null() {
            return Err(Error {
                code: sys::MPV_ERROR_PROPERTY_UNAVAILABLE,
                context: name.into(),
            });
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sys::mpv_free(p as *mut c_void) };
        Ok(s)
    }

    pub fn observe_property(&self, id: u64, name: &str, format: Format) -> Result<()> {
        let n = cstr(name);
        check(
            unsafe { sys::mpv_observe_property(self.handle, id, n.as_ptr(), format.raw()) },
            || format!("observe_property {name}"),
        )
    }

    /// Waits for the next event. `timeout < 0` waits forever. Returns `None` on timeout.
    pub fn wait_event(&self, timeout: f64) -> Option<Event> {
        let ev = unsafe { &*sys::mpv_wait_event(self.handle, timeout) };
        Some(match ev.event_id {
            sys::MPV_EVENT_NONE => return None,
            sys::MPV_EVENT_SHUTDOWN => Event::Shutdown,
            sys::MPV_EVENT_LOG_MESSAGE => {
                let m = unsafe { &*(ev.data as *const sys::mpv_event_log_message) };
                Event::Log {
                    prefix: lossy(m.prefix),
                    level: lossy(m.level),
                    text: lossy(m.text),
                }
            }
            sys::MPV_EVENT_START_FILE => Event::StartFile,
            sys::MPV_EVENT_END_FILE => {
                let e = unsafe { &*(ev.data as *const sys::mpv_event_end_file) };
                let reason = match e.reason {
                    sys::MPV_END_FILE_REASON_EOF => EndReason::Eof,
                    sys::MPV_END_FILE_REASON_STOP => EndReason::Stop,
                    sys::MPV_END_FILE_REASON_QUIT => EndReason::Quit,
                    sys::MPV_END_FILE_REASON_ERROR => EndReason::Error,
                    sys::MPV_END_FILE_REASON_REDIRECT => EndReason::Redirect,
                    other => EndReason::Other(other),
                };
                Event::EndFile {
                    reason,
                    error: e.error,
                }
            }
            sys::MPV_EVENT_FILE_LOADED => Event::FileLoaded,
            sys::MPV_EVENT_VIDEO_RECONFIG => Event::VideoReconfig,
            sys::MPV_EVENT_AUDIO_RECONFIG => Event::AudioReconfig,
            sys::MPV_EVENT_SEEK => Event::Seek,
            sys::MPV_EVENT_PLAYBACK_RESTART => Event::PlaybackRestart,
            sys::MPV_EVENT_PROPERTY_CHANGE => {
                let p = unsafe { &*(ev.data as *const sys::mpv_event_property) };
                let value = if p.data.is_null() {
                    Value::None
                } else {
                    unsafe {
                        match p.format {
                            sys::MPV_FORMAT_FLAG => Value::Flag(*(p.data as *const c_int) != 0),
                            sys::MPV_FORMAT_INT64 => Value::Int(*(p.data as *const i64)),
                            sys::MPV_FORMAT_DOUBLE => Value::Double(*(p.data as *const f64)),
                            sys::MPV_FORMAT_STRING => {
                                Value::Str(lossy(*(p.data as *const *const c_char)))
                            }
                            _ => Value::None,
                        }
                    }
                };
                Event::PropertyChange {
                    id: ev.reply_userdata,
                    name: lossy(p.name),
                    value,
                }
            }
            other => Event::Other(other),
        })
    }
}

fn lossy(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        unsafe { sys::mpv_terminate_destroy(self.handle) }
    }
}
