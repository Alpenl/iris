//! Thin, hand-written bindings to libmpv (client API 2.x and render API).
//!
//! Only what Iris uses is bound. `sys` mirrors the C headers; the rest is a
//! small safe layer: [`Mpv`] for the client handle and [`render::RenderContext`]
//! for the render API.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::ptr;

pub mod render;
pub(crate) mod sys;

pub use render::RenderHandle;

#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub context: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: `mpv_error_string` always returns a valid static NUL-terminated string.
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
    CString::new(s).unwrap_or_else(|_| {
        let mut bytes: Vec<u8> = s.bytes().filter(|&b| b != 0).collect();
        bytes.push(0);
        CString::from_vec_with_nul(bytes).unwrap_or_default()
    })
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

// SAFETY: libmpv documents `mpv_handle` as thread-safe across threads.
unsafe impl Send for Mpv {}
// SAFETY: libmpv synchronizes concurrent client API calls on `mpv_handle` internally.
unsafe impl Sync for Mpv {}

impl Mpv {
    /// Creates an uninitialized handle. Set options, then call [`Mpv::initialize`].
    pub fn create() -> Result<Self> {
        // SAFETY: `mpv_create` takes no arguments and returns a new handle or NULL on OOM.
        let handle = unsafe { sys::mpv_create() };
        if handle.is_null() {
            return Err(Error {
                code: sys::MPV_ERROR_NOMEM,
                context: "mpv_create".into(),
            });
        }
        Ok(Self { handle })
    }

    pub(crate) fn raw(&self) -> *mut sys::mpv_handle {
        self.handle
    }

    pub fn set_option(&self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (cstr(name), cstr(value));
        // SAFETY: `self.handle` is valid and `n`, `v` are NUL-terminated `CString`s.
        check(
            unsafe { sys::mpv_set_option_string(self.handle, n.as_ptr(), v.as_ptr()) },
            || format!("set_option {name}={value}"),
        )
    }

    pub fn initialize(&self) -> Result<()> {
        // SAFETY: `self.handle` is a valid `mpv_handle` owned by `self`.
        check(unsafe { sys::mpv_initialize(self.handle) }, || {
            "mpv_initialize".into()
        })
    }

    pub fn request_log_messages(&self, min_level: &str) -> Result<()> {
        let l = cstr(min_level);
        // SAFETY: `self.handle` is valid and `l` is a NUL-terminated `CString`.
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
        // SAFETY: `self.handle` is valid and `ptrs` is a NULL-terminated array of C strings
        // kept alive by `owned` for the duration of the call.
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
        // SAFETY: `self.handle` is valid and `ptrs` is a NULL-terminated array of C strings
        // kept alive by `owned` until `mpv_command_async` returns.
        check(
            unsafe { sys::mpv_command_async(self.handle, 0, ptrs.as_mut_ptr()) },
            || format!("command_async {args:?}"),
        )
    }

    /// Executes an asynchronous `loadfile` command via `mpv_command_node_async`
    /// (`MPV_FORMAT_NODE_MAP`), passing per-file options as structured key-value
    /// entries (supporting repeated keys such as `"http-header-fields-append"`
    /// and `"sub-files-append"` without string-escaping or injection hazards).
    pub fn loadfile_with_options(
        &self,
        url: &str,
        flags: &str,
        options: &[(&str, &str)],
    ) -> Result<()> {
        let opt_num = c_int::try_from(options.len()).map_err(|_| Error {
            code: sys::MPV_ERROR_INVALID_PARAMETER,
            context: "loadfile_with_options: too many options".into(),
        })?;

        let cmd_key = cstr("_name");
        let cmd_val = cstr("loadfile");
        let url_key = cstr("url");
        let url_val = cstr(url);
        let flags_key = cstr("flags");
        let flags_val = cstr(flags);
        let opts_key = cstr("options");

        let opt_cstrs: Vec<(CString, CString)> =
            options.iter().map(|&(k, v)| (cstr(k), cstr(v))).collect();
        let mut opt_keys: Vec<*mut c_char> = opt_cstrs
            .iter()
            .map(|(k, _)| k.as_ptr().cast_mut())
            .collect();
        let mut opt_values: Vec<sys::mpv_node> = opt_cstrs
            .iter()
            .map(|(_, v)| sys::mpv_node {
                u: sys::mpv_node_u {
                    string: v.as_ptr().cast_mut(),
                },
                format: sys::MPV_FORMAT_STRING,
            })
            .collect();
        let mut opt_list = sys::mpv_node_list {
            num: opt_num,
            values: opt_values.as_mut_ptr(),
            keys: opt_keys.as_mut_ptr(),
        };

        let mut root_keys: Vec<*mut c_char> = Vec::with_capacity(4);
        let mut root_values: Vec<sys::mpv_node> = Vec::with_capacity(4);

        root_keys.push(cmd_key.as_ptr().cast_mut());
        root_values.push(sys::mpv_node {
            u: sys::mpv_node_u {
                string: cmd_val.as_ptr().cast_mut(),
            },
            format: sys::MPV_FORMAT_STRING,
        });

        root_keys.push(url_key.as_ptr().cast_mut());
        root_values.push(sys::mpv_node {
            u: sys::mpv_node_u {
                string: url_val.as_ptr().cast_mut(),
            },
            format: sys::MPV_FORMAT_STRING,
        });

        root_keys.push(flags_key.as_ptr().cast_mut());
        root_values.push(sys::mpv_node {
            u: sys::mpv_node_u {
                string: flags_val.as_ptr().cast_mut(),
            },
            format: sys::MPV_FORMAT_STRING,
        });

        if !options.is_empty() {
            root_keys.push(opts_key.as_ptr().cast_mut());
            root_values.push(sys::mpv_node {
                u: sys::mpv_node_u {
                    list: &raw mut opt_list,
                },
                format: sys::MPV_FORMAT_NODE_MAP,
            });
        }

        let mut root_list = sys::mpv_node_list {
            num: root_values.len() as c_int,
            values: root_values.as_mut_ptr(),
            keys: root_keys.as_mut_ptr(),
        };
        let mut root_node = sys::mpv_node {
            u: sys::mpv_node_u {
                list: &raw mut root_list,
            },
            format: sys::MPV_FORMAT_NODE_MAP,
        };

        // SAFETY: `self.handle` is valid, and `root_node` plus all referenced `mpv_node_list`s,
        // `Vec` buffers, and `CString`s stay alive and unmoved until `mpv_command_node_async` returns.
        check(
            unsafe { sys::mpv_command_node_async(self.handle, 0, &raw mut root_node) },
            || format!("loadfile_with_options {url}"),
        )
    }

    pub fn set_property_string(&self, name: &str, value: &str) -> Result<()> {
        with_cstr(name, |n| {
            with_cstr(value, |v| {
                // SAFETY: `self.handle` is valid and `n`, `v` are valid NUL-terminated C strings.
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
            // SAFETY: `self.handle` is valid, `n` is a NUL-terminated C string, and `value`
            // points to a live `f64` matching `MPV_FORMAT_DOUBLE`.
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
            // SAFETY: `self.handle` is valid, `n` is a NUL-terminated C string, and `v`
            // is a writable `f64` out-parameter matching `MPV_FORMAT_DOUBLE`.
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
        // SAFETY: `self.handle` is valid and `n` is a NUL-terminated C string.
        let p = with_cstr(name, |n| unsafe {
            sys::mpv_get_property_string(self.handle, n)
        });
        if p.is_null() {
            return Err(Error {
                code: sys::MPV_ERROR_PROPERTY_UNAVAILABLE,
                context: name.into(),
            });
        }
        // SAFETY: `p` is non-null and points to a NUL-terminated string allocated by libmpv.
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        // SAFETY: `p` was returned by `mpv_get_property_string` and is freed once here.
        unsafe { sys::mpv_free(p as *mut c_void) };
        Ok(s)
    }

    pub fn observe_property(&self, id: u64, name: &str, format: Format) -> Result<()> {
        let n = cstr(name);
        // SAFETY: `self.handle` is valid and `n` is a NUL-terminated `CString`.
        check(
            unsafe { sys::mpv_observe_property(self.handle, id, n.as_ptr(), format.raw()) },
            || format!("observe_property {name}"),
        )
    }

    /// Waits for the next event. `timeout < 0` waits forever. Returns `None` on timeout.
    pub fn wait_event(&self, timeout: f64) -> Option<Event> {
        // SAFETY: `mpv_wait_event` always returns a non-null `mpv_event` pointer valid
        // until the next `mpv_wait_event` call on this handle.
        let ev = unsafe { &*sys::mpv_wait_event(self.handle, timeout) };
        Some(match ev.event_id {
            sys::MPV_EVENT_NONE => return None,
            sys::MPV_EVENT_SHUTDOWN => Event::Shutdown,
            sys::MPV_EVENT_LOG_MESSAGE => {
                // SAFETY: For `MPV_EVENT_LOG_MESSAGE`, `ev.data` points to `mpv_event_log_message`
                // whose string fields are valid NUL-terminated C strings.
                unsafe {
                    let m = &*(ev.data as *const sys::mpv_event_log_message);
                    Event::Log {
                        prefix: lossy(m.prefix),
                        level: lossy(m.level),
                        text: lossy(m.text),
                    }
                }
            }
            sys::MPV_EVENT_START_FILE => Event::StartFile,
            sys::MPV_EVENT_END_FILE => {
                // SAFETY: For `MPV_EVENT_END_FILE`, `ev.data` points to `mpv_event_end_file`.
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
                // SAFETY: For `MPV_EVENT_PROPERTY_CHANGE`, `ev.data` points to `mpv_event_property`.
                let p = unsafe { &*(ev.data as *const sys::mpv_event_property) };
                let value = if p.data.is_null() {
                    Value::None
                } else {
                    // SAFETY: `p.data` is non-null and its layout is determined by `p.format` per `client.h`.
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
                // SAFETY: `p.name` is either null or a valid NUL-terminated C string from mpv.
                let name = unsafe { lossy(p.name) };
                Event::PropertyChange {
                    id: ev.reply_userdata,
                    name,
                    value,
                }
            }
            other => Event::Other(other),
        })
    }
}

/// Converts a nullable C string pointer from an mpv event into an owned `String`.
///
/// # Safety
/// `p` must be either null or a valid pointer to a NUL-terminated C string.
unsafe fn lossy(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: Caller guarantees `p` points to a valid NUL-terminated C string when non-null.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was created by `mpv_create` and is destroyed once on drop.
        unsafe { sys::mpv_terminate_destroy(self.handle) }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn roundtrip_with_cstr(input: &str) -> String {
        with_cstr(input, |ptr| {
            assert!(!ptr.is_null());
            // SAFETY: `with_cstr` always passes a valid NUL-terminated C string for the callback duration.
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        })
    }

    #[test]
    fn with_cstr_handles_short_boundary_and_interior_nul() {
        assert_eq!(roundtrip_with_cstr(""), "");
        assert_eq!(roundtrip_with_cstr("volume"), "volume");

        let s127 = "a".repeat(127);
        assert_eq!(roundtrip_with_cstr(&s127), s127);

        let s128 = "b".repeat(128);
        assert_eq!(roundtrip_with_cstr(&s128), s128);

        let s200 = "c".repeat(200);
        assert_eq!(roundtrip_with_cstr(&s200), s200);

        assert_eq!(roundtrip_with_cstr("\0"), "");
        assert_eq!(roundtrip_with_cstr("ab\0cd\0ef"), "abcdef");

        let long_with_nul = format!("{}\0{}", "x".repeat(100), "y".repeat(100));
        let expected_long = format!("{}{}", "x".repeat(100), "y".repeat(100));
        assert_eq!(roundtrip_with_cstr(&long_with_nul), expected_long);
    }

    #[test]
    fn headless_mpv_loadfile_with_options_and_render_handle() -> Result<()> {
        let mpv = Mpv::create()?;
        mpv.set_option("vo", "null")?;
        mpv.set_option("ao", "null")?;
        mpv.set_option("idle", "yes")?;
        mpv.set_option("osc", "no")?;
        mpv.set_option("config", "no")?;
        mpv.initialize()?;

        mpv.loadfile_with_options(
            "memory://",
            "replace",
            &[
                ("http-header-fields-append", "Authorization: Bearer tok"),
                ("http-header-fields-append", "Cookie: a=1, b=2"),
            ],
        )?;
        mpv.loadfile_with_options("memory://", "replace", &[])?;

        let mpv = Arc::new(mpv);
        let render_handle = RenderHandle::new(Arc::clone(&mpv));
        render_handle.set_display_color_target("bt.709", "bt.1886")?;
        assert_eq!(mpv.get_property_string("target-prim")?, "bt.709");
        assert_eq!(mpv.get_property_string("target-trc")?, "bt.1886");
        render_handle.set_display_icc_profile("")?;

        Ok(())
    }
}
