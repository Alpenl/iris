//! Background libmpv event pump, load/preloaded hooks, and event translation.

use std::cell::Cell;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Mpv, Value};
use serde_json::json;

use crate::logging::{self, EventLevel};
use iris_net::concurrency::{CancelToken, ThreadRole, spawn_thread};

use super::super::ScopedStream;
use super::super::model::{SessionEvent, SessionUpdate};
use super::super::recovery::VideoRecoveryHandle;
use super::super::relay::Service;
use super::super::scoped;
use super::references::{disable_hls, enable_hls};

pub(super) const HOOK_LOAD: u64 = 1;
pub(super) const HOOK_PRELOADED: u64 = 2;

pub(super) mod prop {
    pub const TIME_POS: u64 = 1;
    pub const DURATION: u64 = 2;
    pub const PAUSE: u64 = 3;
    pub const VOLUME: u64 = 4;
    pub const SPEED: u64 = 5;
    pub const VIDEO_W: u64 = 6;
    pub const VIDEO_H: u64 = 7;
    pub const HWDEC: u64 = 8;
    pub const SUB_VISIBLE: u64 = 9;
    pub const EOF: u64 = 10;
    pub const AUDIO: u64 = 11;
    pub const ROTATION: u64 = 12;
    pub const MUTE: u64 = 13;
    pub const SUB: u64 = 14;
    pub const TRACK_COUNT: u64 = 15;
    pub const LOOP_FILE: u64 = 16;
    pub const SUB_DELAY: u64 = 17;
    pub const CHAPTER: u64 = 18;
    pub const CHAPTER_COUNT: u64 = 19;
    pub const PAUSED_FOR_CACHE: u64 = 20;
    pub const CACHE_DURATION: u64 = 21;
    pub const SOURCE_ROTATION: u64 = 22;
    pub const AB_START: u64 = 23;
    pub const AB_END: u64 = 24;
}

pub(in crate::player) struct EventPump {
    cancel: CancelToken,
    relay: Arc<Mutex<Weak<Service>>>,
    scoped: Arc<Mutex<Weak<dyn ScopedStream>>>,
    recovery: VideoRecoveryHandle,
    thread: Cell<Option<JoinHandle<()>>>,
}

impl EventPump {
    pub(in crate::player) fn set_scoped(
        &self,
        scoped: Weak<dyn ScopedStream>,
    ) -> Weak<dyn ScopedStream> {
        std::mem::replace(
            &mut *self.scoped.lock().unwrap_or_else(PoisonError::into_inner),
            scoped,
        )
    }

    pub(in crate::player) fn set_relay(&self, relay: Weak<Service>) -> Weak<Service> {
        std::mem::replace(
            &mut *self.relay.lock().unwrap_or_else(PoisonError::into_inner),
            relay,
        )
    }

    pub(in crate::player) fn stop(&self, mpv: &Mpv) {
        if let Some(thread) = self.thread.take() {
            // Continue any in-flight load hook until mpv acknowledges quit.
            if mpv.command_async(&["quit"]).is_err() {
                self.cancel.cancel();
                self.recovery.on_shutdown();
                mpv.wakeup();
            }
            let _ = thread.join();
        }
    }
}

pub(in crate::player) fn active_playlist_entry_id(mpv: &Mpv) -> Option<i64> {
    let pos: i64 = mpv
        .get_property_string("playlist-playing-pos")
        .ok()?
        .parse()
        .ok()
        .filter(|&p| p >= 0)?;
    mpv.get_property_string(&format!("playlist/{pos}/id"))
        .ok()?
        .parse()
        .ok()
}

pub(in crate::player) fn spawn_event_pump(
    mpv: Arc<Mpv>,
    recovery: VideoRecoveryHandle,
) -> Result<(EventPump, UnboundedReceiver<SessionUpdate>)> {
    // Unbounded on purpose. mpv coalesces property changes (one pending change
    // per observed property), log lines never reach `tx`, and the remaining
    // events follow playlist and user actions, so the inflow while the UI is
    // slow is limited by mpv's own update rate. A bounded channel would have to
    // either block this thread, which also owns `continue_hook` and is joined
    // by `EventPump::stop` on the UI thread (the thread that drains `rx`), or
    // drop updates such as `EndFile` and `CommandReply` that the session state
    // relies on.
    let (tx, rx) = unbounded();
    let cancel = CancelToken::new();
    let stopped = cancel.clone();
    let relay = Arc::new(Mutex::new(Weak::<Service>::new()));
    let source = relay.clone();
    let scoped_ref = Arc::new(Mutex::new(scoped::empty_scoped()));
    let scoped_source = scoped_ref.clone();
    let pump_recovery = recovery.clone();
    let thread = spawn_thread(ThreadRole::MpvEventPump, move || {
        let mut playing = None;
        let mut hls = false;
        let mut restricted = false;
        let mut last_decode_burst: Option<Instant> = None;
        let mut suppressed_decode_errors: u32 = 0;
        while !stopped.is_cancelled() {
            let Some(raw) = mpv.wait_event(-1.0) else {
                continue;
            };
            let scope = match &raw {
                Event::Shutdown => {
                    pump_recovery.on_shutdown();
                    break;
                }
                Event::CommandReply { .. } => None,
                Event::GetPropertyReply {
                    id, error, value, ..
                } => {
                    pump_recovery.on_property_reply(*id, *error, value);
                    continue;
                }
                Event::StartFile(id) => {
                    hls = false;
                    restricted = false;
                    playing = Some(*id);
                    pump_recovery.on_start_file(*id);
                    continue;
                }
                Event::EndFile {
                    playlist_entry_id, ..
                } => {
                    pump_recovery.on_end_file(*playlist_entry_id);
                    if playing == Some(*playlist_entry_id) {
                        playing = None;
                    }
                    Some(*playlist_entry_id)
                }
                Event::Hook { userdata, id } => {
                    match *userdata {
                        HOOK_LOAD => {
                            restricted = scoped_source
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .upgrade()
                                .is_some_and(|session| {
                                    session.root_url().ok()
                                        == mpv.get_property_string("stream-open-filename").ok()
                                });
                            let service = source
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .upgrade();
                            if let Some(service) = service {
                                match enable_hls(&mpv, &service) {
                                    Ok(prepared) => hls = prepared,
                                    Err(_) => {
                                        let _ = disable_hls(&mpv);
                                        log::warn!("could not prepare scoped HLS playback");
                                    }
                                }
                            }
                        }
                        HOOK_PRELOADED if restricted => {
                            if scoped::restore_attachment_policy(&mpv).is_err() {
                                log::error!("could not restore scoped attachment policy");
                                let _ = mpv.command_async(&["stop"]);
                            }
                            restricted = false;
                        }
                        HOOK_PRELOADED if hls => {
                            // The HLS demuxer captures its open policy. Future
                            // attachments must see the restrictive defaults.
                            if disable_hls(&mpv).is_err() {
                                log::error!("could not restore mpv reference policy");
                                let _ = mpv.command_async(&["stop"]);
                            }
                            hls = false;
                        }
                        _ => (),
                    }
                    // SAFETY: This event came from this handle's wait_event call, and this
                    // is its only continuation after completing the hook's work.
                    let _ = unsafe { mpv.continue_hook(*id) };
                    continue;
                }
                Event::QueueOverflow => {
                    playing = active_playlist_entry_id(&mpv);
                    pump_recovery.on_overflow(playing);
                    log::warn!("mpv event queue overflow; resyncing active entry {playing:?}");
                    None
                }
                Event::Log {
                    prefix,
                    level,
                    text,
                } => {
                    let detail = logging::redact_text(text.trim_end());
                    let mapped = match level.as_str() {
                        "fatal" | "error" => log::Level::Error,
                        "warn" => log::Level::Warn,
                        "info" => log::Level::Info,
                        _ => log::Level::Debug,
                    };
                    log::log!(target: "iris::mpv", mapped, "{prefix}/{level}: {detail}");
                    if prefix == "vd" && detail.contains("Attempting next decoding method") {
                        logging::event(
                            EventLevel::Warn,
                            "player",
                            "decode_fallback",
                            json!({ "prefix": prefix, "detail": detail }),
                        );
                    } else if matches!(prefix.as_str(), "vd" | "vo" | "d3d11")
                        || (prefix == "ffmpeg" && level == "error")
                    {
                        let now = Instant::now();
                        let due = last_decode_burst
                            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5));
                        if due {
                            let skipped = std::mem::take(&mut suppressed_decode_errors);
                            last_decode_burst = Some(now);
                            logging::event(
                                EventLevel::Warn,
                                "player",
                                "mpv_diagnostic",
                                json!({
                                    "prefix": prefix,
                                    "mpv_level": level,
                                    "detail": detail,
                                    "suppressed_since_last": skipped,
                                }),
                            );
                        } else {
                            suppressed_decode_errors = suppressed_decode_errors.saturating_add(1);
                        }
                    }
                    continue;
                }
                Event::PropertyChange { id, .. }
                    if matches!(
                        *id,
                        prop::PAUSE | prop::VOLUME | prop::MUTE | prop::SPEED | prop::SUB_VISIBLE
                    ) =>
                {
                    None
                }
                _ => match playing {
                    Some(id) => Some(id),
                    None => continue,
                },
            };
            if let Some(event) = SessionEvent::from_backend(raw) {
                let _ = tx.unbounded_send(SessionUpdate {
                    playlist_entry_id: scope,
                    event,
                });
            }
        }
    })?;
    Ok((
        EventPump {
            cancel,
            relay,
            scoped: scoped_ref,
            recovery,
            thread: Cell::new(Some(thread)),
        },
        rx,
    ))
}

impl SessionEvent {
    pub(super) fn from_backend(event: Event) -> Option<Self> {
        Some(match event {
            Event::Seek => Self::Seek,
            Event::CommandReply { id, error } => Self::CommandReply { id, error },
            Event::PropertyChange { id, value, .. } => match (id, value) {
                (prop::TIME_POS, Value::Double(v)) => Self::Position(v),
                (prop::TIME_POS, Value::None) => Self::Position(0.),
                (prop::DURATION, Value::Double(v)) => Self::Duration(v),
                (prop::PAUSE, Value::Flag(v)) => Self::Paused(v),
                (prop::VOLUME, Value::Double(v)) => Self::Volume(v),
                (prop::MUTE, Value::Flag(v)) => Self::Muted(v),
                (prop::SPEED, Value::Double(v)) => Self::Speed(v),
                (prop::VIDEO_W, Value::Int(v)) => Self::Width(v),
                (prop::VIDEO_H, Value::Int(v)) => Self::Height(v),
                (prop::HWDEC, Value::Str(v)) => Self::Hardware(v),
                (prop::HWDEC, Value::None) => Self::Hardware(String::new()),
                (prop::SUB_VISIBLE, Value::Flag(v)) => Self::Subtitles(v),
                (prop::SUB, Value::Str(v)) => Self::SubtitleTrack(v),
                (prop::SUB, Value::None) => Self::SubtitleTrack(String::new()),
                (prop::SUB_DELAY, Value::Double(v)) => Self::SubDelay(v),
                (prop::EOF, Value::Flag(v)) => Self::Ended(v),
                (prop::AUDIO, Value::Str(v)) => Self::Audio(v),
                (prop::AUDIO, Value::None) => Self::Audio(String::new()),
                (prop::ROTATION, Value::Int(v)) => Self::Rotation(v),
                (prop::SOURCE_ROTATION, Value::Int(v)) => Self::SourceRotation(v),
                (prop::TRACK_COUNT, Value::Int(_)) => Self::TracksChanged,
                (prop::LOOP_FILE, Value::Str(v)) => Self::LoopFile(v == "inf" || v == "yes"),
                (prop::AB_START, Value::Str(v)) => {
                    Self::AbStart(v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.))
                }
                (prop::AB_END, Value::Str(v)) => {
                    Self::AbEnd(v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.))
                }
                (prop::CHAPTER, Value::Int(v)) => Self::Chapter(v),
                (prop::CHAPTER, Value::None) => Self::Chapter(-1),
                (prop::CHAPTER_COUNT, Value::Int(_)) => Self::ChaptersChanged,
                (prop::PAUSED_FOR_CACHE, Value::Flag(v)) => Self::Buffering(v),
                (prop::PAUSED_FOR_CACHE, Value::None) => Self::Buffering(false),
                (prop::CACHE_DURATION, Value::Double(v)) => Self::CacheDuration(v),
                (prop::CACHE_DURATION, Value::None) => Self::CacheDuration(0.),
                _ => return None,
            },
            Event::FileLoaded => Self::Loaded,
            Event::VideoReconfig => Self::Reconfigured,
            Event::QueueOverflow => Self::QueueOverflow,
            Event::EndFile {
                reason: iris_mpv::EndReason::Error,
                error,
                ..
            } => Self::Error(error),
            _ => return None,
        })
    }
}
