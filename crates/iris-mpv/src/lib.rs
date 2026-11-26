//! Thin, hand-written bindings to libmpv (client API 2.x and render API).
//!
//! Only what Iris uses is bound. `sys` mirrors the C headers; the rest is a
//! small safe layer: [`Mpv`] for the client handle and [`render::RenderContext`]
//! for the render API.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::ptr;
use std::sync::Mutex;

mod event;
pub mod render;
mod reply;
pub(crate) mod sys;

pub use event::{EndReason, Event, Format, Value};
pub use render::RenderHandle;

#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub context: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Clamp `i32::MIN` before FFI: `mpv_error_string` negates `error` in C (`client.c`).
        let safe_code = self.code.max(-1024);
        // SAFETY: `mpv_error_string` always returns a valid static NUL-terminated string.
        let msg = unsafe { CStr::from_ptr(sys::mpv_error_string(safe_code)) };
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

/// An mpv client handle. The client API is thread-safe. [`Mpv::wait_event`]
/// additionally serializes access to the handle's borrowed event storage.
pub struct Mpv {
    handle: *mut sys::mpv_handle,
    wait_lock: Mutex<()>,
    #[cfg(target_os = "windows")]
    video_recovery: Mutex<render::VideoRecovery>,
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
        Ok(Self {
            handle,
            wait_lock: Mutex::new(()),
            #[cfg(target_os = "windows")]
            video_recovery: Mutex::default(),
        })
    }

    pub(crate) fn raw(&self) -> *mut sys::mpv_handle {
        self.handle
    }

    pub fn set_option(&mut self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (cstr(name), cstr(value));
        // SAFETY: `self.handle` is valid and `n`, `v` are NUL-terminated `CString`s.
        check(
            unsafe { sys::mpv_set_option_string(self.handle, n.as_ptr(), v.as_ptr()) },
            || format!("set_option {name}"),
        )
    }

    pub fn initialize(&mut self) -> Result<()> {
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
            || format!("command {}", args.first().copied().unwrap_or("<empty>")),
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
            || {
                format!(
                    "command_async {}",
                    args.first().copied().unwrap_or("<empty>")
                )
            },
        )
    }

    /// Queues playback with structured per-file options and returns the new
    /// playlist entry ID. Only the command is synchronous; opening media remains
    /// asynchronous in the player core.
    pub fn loadfile_with_options(
        &self,
        url: &str,
        flags: &str,
        options: &[(&str, &str)],
    ) -> Result<i64> {
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

        let mut result = sys::mpv_node {
            u: sys::mpv_node_u { int64: 0 },
            format: sys::MPV_FORMAT_NONE,
        };
        // SAFETY: The option tree stays alive through the command. mpv fills
        // the result node on success and transfers ownership to this caller.
        check(
            unsafe { sys::mpv_command_node(self.handle, &raw mut root_node, &raw mut result) },
            || "loadfile_with_options".into(),
        )?;
        // SAFETY: A successful command returned a valid result node.
        let id = unsafe { event::load_entry_id(&result) };
        // SAFETY: The result and its contents are freed exactly once here.
        unsafe { sys::mpv_free_node_contents(&raw mut result) };
        id.ok_or_else(|| Error {
            code: sys::MPV_ERROR_PROPERTY_UNAVAILABLE,
            context: "loadfile did not return a playlist entry ID".into(),
        })
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

    pub(crate) fn set_property_string_async(&self, name: &str, value: &str) -> Result<()> {
        with_cstr(name, |n| {
            with_cstr(value, |mut v| {
                // SAFETY: The live handle queues a copy of the NUL-terminated value.
                // MPV_FORMAT_STRING takes the address of the C string pointer.
                check(
                    unsafe {
                        sys::mpv_set_property_async(
                            self.handle,
                            0,
                            n,
                            sys::MPV_FORMAT_STRING,
                            (&mut v as *mut *const c_char).cast(),
                        )
                    },
                    || format!("set_property_async {name}"),
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

    pub fn add_hook(&self, userdata: u64, name: &str) -> Result<()> {
        with_cstr(name, |n| {
            // SAFETY: mpv copies the C hook name while it is valid.
            check(
                unsafe { sys::mpv_hook_add(self.handle, userdata, n, 0) },
                || format!("add_hook {name}"),
            )
        })
    }

    /// Continues playback after handling a hook event.
    ///
    /// # Safety
    /// `id` must be the `id` from an [`Event::Hook`] delivered to this handle.
    /// Call this exactly once for that event. Using a different handle or ID,
    /// or calling it again even after an error, is undefined behavior.
    pub unsafe fn continue_hook(&self, id: u64) -> Result<()> {
        // SAFETY: The caller guarantees the ID belongs to an uncontinued hook on this handle.
        check(unsafe { sys::mpv_hook_continue(self.handle, id) }, || {
            "continue_hook".into()
        })
    }

    /// Interrupts a current or subsequent wait on this handle.
    pub fn wakeup(&self) {
        // SAFETY: The handle is live and mpv_wakeup is thread-safe.
        unsafe { sys::mpv_wakeup(self.handle) }
    }

    /// Unblocks any pending device-recovery video track query if shutdown skips normal events.
    #[cfg(target_os = "windows")]
    pub fn abort_video_recovery(&self) {
        self.video_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_event(&Event::Shutdown);
    }

    /// Restores the active playlist entry ID in `VideoRecovery` after `Event::QueueOverflow`.
    #[cfg(target_os = "windows")]
    pub fn reseed_video_recovery(&self, entry: Option<i64>) {
        self.video_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_entry(entry);
    }

    /// Waits for the next event. `timeout < 0` waits forever. Returns `None` on timeout.
    pub fn wait_event(&self, timeout: f64) -> Option<Event> {
        let _wait = self.wait_lock.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `mpv_wait_event` returns an `mpv_event` pointer valid until the next
        // `mpv_wait_event` call on this handle; guard against unexpected null defensively.
        let raw = unsafe { sys::mpv_wait_event(self.handle, timeout) };
        if raw.is_null() {
            return None;
        }
        let ev = unsafe { &*raw };
        #[cfg(target_os = "windows")]
        if ev.event_id == sys::MPV_EVENT_GET_PROPERTY_REPLY && ev.reply_userdata != 0 {
            let vid = if ev.error >= 0 && !ev.data.is_null() {
                // SAFETY: A successful GET_PROPERTY_REPLY contains mpv_event_property.
                let prop = unsafe { &*ev.data.cast::<sys::mpv_event_property>() };
                if prop.format == sys::MPV_FORMAT_STRING && !prop.data.is_null() {
                    // SAFETY: STRING data contains a C-string pointer valid for this event.
                    let value = unsafe { *prop.data.cast::<*const c_char>() };
                    if value.is_null() {
                        None
                    } else {
                        // SAFETY: libmpv provided a non-null NUL-terminated string.
                        unsafe { CStr::from_ptr(value) }
                            .to_str()
                            .ok()
                            .and_then(|s| s.parse::<i64>().ok())
                            .filter(|&id| id > 0)
                    }
                } else {
                    None
                }
            } else {
                None
            };
            self.video_recovery
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reply(ev.reply_userdata, vid);
        }
        // SAFETY: `ev` is valid for the duration of this `wait_lock` guard.
        let event = unsafe { event::decode_event(ev) }?;
        #[cfg(target_os = "windows")]
        match event {
            Event::StartFile(_)
            | Event::EndFile { .. }
            | Event::QueueOverflow
            | Event::Shutdown => {
                self.video_recovery
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .on_event(&event);
            }
            _ => (),
        }
        Some(event)
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
    use std::ffi::CStr;
    use std::sync::Arc;

    use super::{Error, Mpv, RenderHandle, Result, with_cstr};

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
    fn error_display_clamps_i32_min_without_c_negation_overflow() {
        let err = Error {
            code: i32::MIN,
            context: "min-code".into(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains(&i32::MIN.to_string()));
        assert!(rendered.contains("min-code"));
    }

    #[test]
    fn headless_mpv_loadfile_with_options_and_render_handle() -> Result<()> {
        let mut mpv = Mpv::create()?;
        mpv.set_option("vo", "null")?;
        mpv.set_option("ao", "null")?;
        mpv.set_option("idle", "yes")?;
        match mpv.set_option("osc", "no") {
            Ok(()) => {}
            Err(e) if e.code == -5 => {}
            Err(e) => return Err(e),
        }
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
