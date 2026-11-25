//! Rotating diagnostics, panic capture, privacy redaction, and telemetry-ready structured events.
//!
//! All data remains strictly local (`iris.log` + `events.jsonl` with bounded rotation).
//! Future opt-in telemetry can attach via [`register_sink`] without modifying instrumentation call sites.

mod event;
mod redact;
mod writer;

use std::backtrace::Backtrace;
use std::io::{self, Write};
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::{Arc, Once, OnceLock};

use serde_json::{Value, json};

pub use event::{EVENT_SCHEMA_VERSION, EventLevel, TelemetryEvent, TelemetrySink};
pub use redact::{redact_json, redact_text, redact_url, url_host};

use event::{JsonlFileSink, global_bus};
use writer::{DEFAULT_MAX_BACKUPS, DEFAULT_MAX_FILE_BYTES, RotatingFile};

static TEXT_LOG: OnceLock<Arc<RotatingFile>> = OnceLock::new();

struct DualLogOutput {
    file: Arc<RotatingFile>,
}

impl Write for DualLogOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(bytes);
        let flush = bytes.windows(5).any(|w| w == b"WARN " || w == b"ERROR");
        let _ = self.file.write_bytes(bytes, flush);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.file.flush();
        Ok(())
    }
}

/// Ephemeral random 128-bit hex identifier for the current process session.
pub fn session_id() -> &'static str {
    global_bus().session_id()
}

/// Registers an additional telemetry sink (e.g. a future opt-in uploader or test collector).
pub fn register_sink(sink: Arc<dyn TelemetrySink>) {
    global_bus().register_sink(sink);
}

/// Path to the active human-readable rotating log file (`iris.log`).
pub fn log_path() -> Option<PathBuf> {
    std::env::var_os("IRIS_LOG").map(PathBuf::from).or_else(|| {
        if let Some(dir) = std::env::var_os("IRIS_LOG_DIR").filter(|dir| !dir.is_empty()) {
            return Some(PathBuf::from(dir).join("iris.log"));
        }
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Iris"));
        #[cfg(target_os = "macos")]
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join("Library/Logs/Iris"));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let base: Option<PathBuf> = None;
        base.map(|p| p.join("iris.log"))
    })
}

/// Path to the rotating structured JSONL diagnostic event stream (`events.jsonl`).
pub fn events_log_path() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("IRIS_EVENTS_LOG").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(override_path));
    }
    log_path().map(|p| p.with_file_name("events.jsonl"))
}

/// Initializes rotating text logs, structured JSONL event sink, and global panic hook.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let mut builder = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("iris=info,iris_core=info,iris_mpv=info"),
        );
        builder.format_timestamp_millis();

        let text_log_ready = if let Some(path) = log_path() {
            match RotatingFile::open(
                path.clone(),
                DEFAULT_MAX_FILE_BYTES,
                DEFAULT_MAX_BACKUPS,
                true,
            ) {
                Ok(rotating) => {
                    let rotating = Arc::new(rotating);
                    let _ = TEXT_LOG.set(rotating.clone());
                    builder.target(env_logger::Target::Pipe(Box::new(DualLogOutput {
                        file: rotating,
                    })));
                    true
                }
                Err(e) => {
                    eprintln!("Iris: cannot open log {}: {e}", path.display());
                    false
                }
            }
        } else {
            false
        };

        let events_ready = if let Some(events_path) = events_log_path() {
            match RotatingFile::open(
                events_path.clone(),
                DEFAULT_MAX_FILE_BYTES,
                DEFAULT_MAX_BACKUPS,
                false,
            ) {
                Ok(rotating) => {
                    register_sink(Arc::new(JsonlFileSink::new(Arc::new(rotating))));
                    true
                }
                Err(e) => {
                    eprintln!("Iris: cannot open event log {}: {e}", events_path.display());
                    false
                }
            }
        } else {
            false
        };

        let _ = builder.try_init();
        install_panic_hook();

        record_info(
            "app",
            "session_start",
            json!({
                "text_log_ready": text_log_ready,
                "events_log_ready": events_ready,
                "schema": EVENT_SCHEMA_VERSION,
            }),
        );
    });
}

/// Records a clean process shutdown and flushes all active log & event sinks.
pub fn shutdown() {
    let uptime_ms = global_bus().uptime_ms();
    emit_event(
        EventLevel::Info,
        "app",
        "session_end",
        json!({ "uptime_ms": uptime_ms }),
    );
    log::info!(
        target: "iris::telemetry",
        "[app.session_end] session={} uptime_ms={uptime_ms}",
        session_id()
    );
    flush();
}

/// Flushes buffered text logs and structured event sinks to disk.
pub fn flush() {
    global_bus().flush();
    if let Some(text) = TEXT_LOG.get() {
        text.flush();
    }
}

/// Emits a structured, automatically redacted event to the local JSONL log and any
/// registered telemetry sinks without emitting a duplicate plain-text log line.
pub fn emit_event(
    level: EventLevel,
    category: &'static str,
    event: &'static str,
    fields: Value,
) -> TelemetryEvent {
    global_bus().emit(level, category, event, fields)
}

/// Emits a structured event AND writes a correlated, redacted summary line to `log`.
pub fn record_event(
    level: EventLevel,
    category: &'static str,
    event: &'static str,
    fields: Value,
) -> TelemetryEvent {
    let emitted = emit_event(level, category, event, fields);
    let summary = serde_json::to_string(&emitted.fields).unwrap_or_else(|_| "{}".into());
    log::log!(
        target: "iris::event",
        level.to_log_level(),
        "[{category}.{event}] seq={} {summary}",
        emitted.seq
    );
    emitted
}

pub fn record_info(category: &'static str, event: &'static str, fields: Value) -> TelemetryEvent {
    record_event(EventLevel::Info, category, event, fields)
}

pub fn record_warn(category: &'static str, event: &'static str, fields: Value) -> TelemetryEvent {
    record_event(EventLevel::Warn, category, event, fields)
}

pub fn record_error(category: &'static str, event: &'static str, fields: Value) -> TelemetryEvent {
    record_event(EventLevel::Error, category, event, fields)
}

/// Convenience structured-event helper for callers without a direct `serde_json` dependency.
pub fn record_kv(
    level: EventLevel,
    category: &'static str,
    event: &'static str,
    fields: &[(&str, &str)],
) -> TelemetryEvent {
    let mut map = serde_json::Map::with_capacity(fields.len());
    for &(k, v) in fields {
        map.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    record_event(level, category, event, Value::Object(map))
}

fn format_panic_payload(info: &PanicHookInfo<'_>) -> (String, String, String) {
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    let location = info.location().map_or_else(
        || "<unknown>".to_owned(),
        |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
    );
    let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    };
    (thread, redact_text(&location), redact_text(&message))
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (thread, location, message) = format_panic_payload(info);
        let raw_bt = Backtrace::force_capture().to_string();
        let mut backtrace = redact_text(&raw_bt);
        if backtrace.len() > 8192 {
            let mut end = 8192;
            while !backtrace.is_char_boundary(end) {
                end -= 1;
            }
            backtrace.truncate(end);
            backtrace.push_str("\n<truncated>");
        }
        emit_event(
            EventLevel::Error,
            "app",
            "panic",
            json!({
                "thread": thread,
                "location": location,
                "message": message,
                "backtrace": backtrace,
            }),
        );
        log::error!(
            target: "iris::panic",
            "panic on thread '{thread}' at {location}: {message}\n{backtrace}"
        );
        flush();
        prev(info);
    }));
}

#[cfg(test)]
mod tests;
