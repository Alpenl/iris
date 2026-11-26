//! Safe event and node decoding helpers for libmpv FFI.

use std::ffi::{CStr, c_char, c_int};

use crate::sys;

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
    pub(crate) fn raw(self) -> sys::mpv_format {
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
    CommandReply {
        id: u64,
        error: i32,
    },
    Log {
        prefix: String,
        level: String,
        text: String,
    },
    StartFile(i64),
    EndFile {
        reason: EndReason,
        error: i32,
        playlist_entry_id: i64,
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
    QueueOverflow,
    Hook {
        userdata: u64,
        id: u64,
    },
    Other(i32),
}

/// Decodes a raw `mpv_event` into an owned [`Event`], returning `None` for
/// `MPV_EVENT_NONE` and falling back to [`Event::Other`] if an expected payload
/// pointer is unexpectedly null.
///
/// # Safety
/// `ev` must come from `mpv_wait_event` (or a valid synthetic test event whose
/// non-null `data` pointer matches `ev.event_id`).
pub(crate) unsafe fn decode_event(ev: &sys::mpv_event) -> Option<Event> {
    let event = match ev.event_id {
        sys::MPV_EVENT_NONE => return None,
        sys::MPV_EVENT_SHUTDOWN => Event::Shutdown,
        sys::MPV_EVENT_COMMAND_REPLY if ev.reply_userdata != 0 => Event::CommandReply {
            id: ev.reply_userdata,
            error: ev.error,
        },
        sys::MPV_EVENT_LOG_MESSAGE => {
            if ev.data.is_null() {
                Event::Other(ev.event_id)
            } else {
                // SAFETY: `ev.data` is non-null and points to `mpv_event_log_message`
                // whose string fields are nullable NUL-terminated C strings.
                unsafe {
                    let m = &*ev.data.cast::<sys::mpv_event_log_message>();
                    Event::Log {
                        prefix: lossy(m.prefix),
                        level: lossy(m.level),
                        text: lossy(m.text),
                    }
                }
            }
        }
        sys::MPV_EVENT_START_FILE => {
            if ev.data.is_null() {
                Event::Other(ev.event_id)
            } else {
                // SAFETY: `ev.data` is non-null and points to `mpv_event_start_file`.
                let data = unsafe { &*ev.data.cast::<sys::mpv_event_start_file>() };
                Event::StartFile(data.playlist_entry_id)
            }
        }
        sys::MPV_EVENT_END_FILE => {
            if ev.data.is_null() {
                Event::Other(ev.event_id)
            } else {
                // SAFETY: `ev.data` is non-null and points to `mpv_event_end_file`.
                let e = unsafe { &*ev.data.cast::<sys::mpv_event_end_file>() };
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
                    playlist_entry_id: e.playlist_entry_id,
                }
            }
        }
        sys::MPV_EVENT_FILE_LOADED => Event::FileLoaded,
        sys::MPV_EVENT_VIDEO_RECONFIG => Event::VideoReconfig,
        sys::MPV_EVENT_AUDIO_RECONFIG => Event::AudioReconfig,
        sys::MPV_EVENT_SEEK => Event::Seek,
        sys::MPV_EVENT_PLAYBACK_RESTART => Event::PlaybackRestart,
        sys::MPV_EVENT_PROPERTY_CHANGE => {
            if ev.data.is_null() {
                Event::Other(ev.event_id)
            } else {
                // SAFETY: `ev.data` is non-null and points to `mpv_event_property`.
                let p = unsafe { &*ev.data.cast::<sys::mpv_event_property>() };
                let value = if p.data.is_null() {
                    Value::None
                } else {
                    // SAFETY: `p.data` is non-null and its layout matches `p.format` per `client.h`.
                    unsafe {
                        match p.format {
                            sys::MPV_FORMAT_FLAG => Value::Flag(*p.data.cast::<c_int>() != 0),
                            sys::MPV_FORMAT_INT64 => Value::Int(*p.data.cast::<i64>()),
                            sys::MPV_FORMAT_DOUBLE => Value::Double(*p.data.cast::<f64>()),
                            sys::MPV_FORMAT_STRING => {
                                Value::Str(lossy(*p.data.cast::<*const c_char>()))
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
        }
        sys::MPV_EVENT_QUEUE_OVERFLOW => Event::QueueOverflow,
        sys::MPV_EVENT_HOOK => {
            if ev.data.is_null() {
                Event::Other(ev.event_id)
            } else {
                // SAFETY: `ev.data` is non-null and points to `mpv_event_hook`.
                let data = unsafe { &*ev.data.cast::<sys::mpv_event_hook>() };
                Event::Hook {
                    userdata: ev.reply_userdata,
                    id: data.id,
                }
            }
        }
        other => Event::Other(other),
    };
    Some(event)
}

/// # Safety
/// `result` must be a valid mpv node returned by a successful command.
pub(crate) unsafe fn load_entry_id(result: &sys::mpv_node) -> Option<i64> {
    if result.format != sys::MPV_FORMAT_NODE_MAP {
        return None;
    }
    // SAFETY: `format == MPV_FORMAT_NODE_MAP` selects the `list` union field.
    let list_ptr = unsafe { result.u.list };
    if list_ptr.is_null() {
        return None;
    }
    // SAFETY: `list_ptr` is non-null and points to a valid `mpv_node_list`.
    let list = unsafe { &*list_ptr };
    let Ok(len) = usize::try_from(list.num) else {
        return None;
    };
    if len == 0 || list.keys.is_null() || list.values.is_null() {
        return None;
    }
    for i in 0..len {
        // SAFETY: `keys` and `values` are non-null arrays of at least `len` elements.
        let key_ptr = unsafe { *list.keys.add(i) };
        if key_ptr.is_null() {
            continue;
        }
        // SAFETY: `key_ptr` is non-null and NUL-terminated; `values.add(i)` is in bounds.
        let key = unsafe { CStr::from_ptr(key_ptr) };
        let value = unsafe { &*list.values.add(i) };
        if key.to_bytes() == b"playlist_entry_id" && value.format == sys::MPV_FORMAT_INT64 {
            return Some(unsafe { value.u.int64 });
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use std::ffi::{CString, c_int};
    use std::ptr;

    use crate::sys;

    use super::{EndReason, Event, Value, decode_event, load_entry_id};

    #[test]
    fn decode_event_guards_null_data_pointers() {
        for event_id in [
            sys::MPV_EVENT_LOG_MESSAGE,
            sys::MPV_EVENT_START_FILE,
            sys::MPV_EVENT_END_FILE,
            sys::MPV_EVENT_PROPERTY_CHANGE,
            sys::MPV_EVENT_HOOK,
        ] {
            let ev = sys::mpv_event {
                event_id,
                error: 0,
                reply_userdata: 42,
                data: ptr::null_mut(),
            };
            let decoded = unsafe { decode_event(&ev) };
            assert!(
                matches!(decoded, Some(Event::Other(id)) if id == event_id),
                "expected Event::Other({event_id}), got {decoded:?}"
            );
        }

        let none_ev = sys::mpv_event {
            event_id: sys::MPV_EVENT_NONE,
            error: 0,
            reply_userdata: 0,
            data: ptr::null_mut(),
        };
        assert!(unsafe { decode_event(&none_ev) }.is_none());
    }

    #[test]
    fn decode_event_decodes_valid_payloads() {
        let mut start = sys::mpv_event_start_file {
            playlist_entry_id: 99,
        };
        let ev = sys::mpv_event {
            event_id: sys::MPV_EVENT_START_FILE,
            error: 0,
            reply_userdata: 0,
            data: (&raw mut start).cast(),
        };
        assert!(matches!(
            unsafe { decode_event(&ev) },
            Some(Event::StartFile(99))
        ));

        let mut end = sys::mpv_event_end_file {
            reason: sys::MPV_END_FILE_REASON_ERROR,
            error: -13,
            playlist_entry_id: 7,
            playlist_insert_id: 0,
            playlist_insert_num_entries: 0,
        };
        let ev = sys::mpv_event {
            event_id: sys::MPV_EVENT_END_FILE,
            error: 0,
            reply_userdata: 0,
            data: (&raw mut end).cast(),
        };
        assert!(matches!(
            unsafe { decode_event(&ev) },
            Some(Event::EndFile {
                reason: EndReason::Error,
                error: -13,
                playlist_entry_id: 7,
            })
        ));

        let prop_name = CString::new("time-pos").unwrap();
        let mut prop = sys::mpv_event_property {
            name: prop_name.as_ptr(),
            format: sys::MPV_FORMAT_DOUBLE,
            data: ptr::null_mut(),
        };
        let ev = sys::mpv_event {
            event_id: sys::MPV_EVENT_PROPERTY_CHANGE,
            error: 0,
            reply_userdata: 1,
            data: (&raw mut prop).cast(),
        };
        assert!(matches!(
            unsafe { decode_event(&ev) },
            Some(Event::PropertyChange {
                id: 1,
                ref name,
                value: Value::None,
            }) if name == "time-pos"
        ));
    }

    #[test]
    fn load_entry_id_guards_null_pointers_and_negative_lengths() {
        let null_list = sys::mpv_node {
            u: sys::mpv_node_u {
                list: ptr::null_mut(),
            },
            format: sys::MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(unsafe { load_entry_id(&null_list) }, None);

        for bad_num in [-1 as c_int, c_int::MIN, 0] {
            let mut list = sys::mpv_node_list {
                num: bad_num,
                values: ptr::null_mut(),
                keys: ptr::null_mut(),
            };
            let node = sys::mpv_node {
                u: sys::mpv_node_u {
                    list: &raw mut list,
                },
                format: sys::MPV_FORMAT_NODE_MAP,
            };
            assert_eq!(unsafe { load_entry_id(&node) }, None);
        }

        let mut null_arrays = sys::mpv_node_list {
            num: 1,
            values: ptr::null_mut(),
            keys: ptr::null_mut(),
        };
        let node = sys::mpv_node {
            u: sys::mpv_node_u {
                list: &raw mut null_arrays,
            },
            format: sys::MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(unsafe { load_entry_id(&node) }, None);

        let key = CString::new("playlist_entry_id").unwrap();
        let mut keys = [ptr::null_mut(), key.as_ptr().cast_mut()];
        let mut values = [
            sys::mpv_node {
                u: sys::mpv_node_u { int64: 1 },
                format: sys::MPV_FORMAT_INT64,
            },
            sys::mpv_node {
                u: sys::mpv_node_u { int64: 77 },
                format: sys::MPV_FORMAT_INT64,
            },
        ];
        let mut list = sys::mpv_node_list {
            num: 2,
            values: values.as_mut_ptr(),
            keys: keys.as_mut_ptr(),
        };
        let node = sys::mpv_node {
            u: sys::mpv_node_u {
                list: &raw mut list,
            },
            format: sys::MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(unsafe { load_entry_id(&node) }, Some(77));
    }
}
