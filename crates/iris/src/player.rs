//! mpv setup and the event pump.

use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_mpv::{Event, Format, Mpv};

/// Observed property ids.
pub mod prop {
    pub const TIME_POS: u64 = 1;
    pub const DURATION: u64 = 2;
    pub const PAUSE: u64 = 3;
    pub const VOLUME: u64 = 4;
    pub const SPEED: u64 = 5;
    pub const VIDEO_W: u64 = 6;
    pub const VIDEO_H: u64 = 7;
    pub const HWDEC: u64 = 8;
    pub const SUB_VISIBLE: u64 = 9;
}

pub fn create() -> Result<Arc<Mpv>> {
    let mpv = Mpv::create()?;
    for (k, v) in [
        ("vo", "libmpv"),
        ("hwdec", "auto-safe"),
        ("keep-open", "yes"),
        ("idle", "yes"),
        ("image-display-duration", "inf"),
        ("osc", "no"),
        // Iris draws its own UI: skip mpv's built-in Lua scripts (one thread
        // and Lua state each) and user scripts.
        ("load-scripts", "no"),
        ("ytdl", "no"),
        ("load-stats-overlay", "no"),
        ("load-console", "no"),
        ("load-select", "no"),
        ("load-positioning", "no"),
        ("load-context-menu", "no"),
        ("load-commands", "no"),
        ("load-auto-profiles", "no"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("config", "no"),
        ("terminal", "no"),
        // Pick Chinese subtitles first, then English; with no match, still show
        // a full (non-forced) track, as PotPlayer does.
        (
            "slang",
            "zh-Hans,zh-CN,chs,sc,zh,chi,zho,zh-Hant,zh-TW,cht,tc,en,eng",
        ),
        ("subs-fallback", "yes"),
    ] {
        mpv.set_option(k, v)?;
    }
    mpv.initialize()?;
    mpv.request_log_messages("warn")?;
    for (id, name, format) in [
        (prop::TIME_POS, "time-pos", Format::Double),
        (prop::DURATION, "duration", Format::Double),
        (prop::PAUSE, "pause", Format::Flag),
        (prop::VOLUME, "volume", Format::Double),
        (prop::SPEED, "speed", Format::Double),
        (prop::VIDEO_W, "video-params/w", Format::Int),
        (prop::VIDEO_H, "video-params/h", Format::Int),
        (prop::HWDEC, "hwdec-current", Format::Str),
        (prop::SUB_VISIBLE, "sub-visibility", Format::Flag),
    ] {
        mpv.observe_property(id, name, format)?;
    }
    Ok(Arc::new(mpv))
}

/// Forwards mpv events from a blocking thread to an async channel. The thread
/// ends (and drops its handle reference) when mpv shuts down.
pub fn spawn_event_pump(mpv: Arc<Mpv>) -> UnboundedReceiver<Event> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("iris-mpv-events".into())
        .spawn(move || {
            loop {
                match mpv.wait_event(-1.0) {
                    Some(Event::Shutdown) => break,
                    Some(Event::Log {
                        prefix,
                        level,
                        text,
                    }) => {
                        eprint!("[mpv/{prefix}] {level}: {text}");
                    }
                    Some(ev) => {
                        if tx.unbounded_send(ev).is_err() {
                            break;
                        }
                    }
                    None => {}
                }
            }
        })
        .expect("spawn mpv event thread");
    rx
}
