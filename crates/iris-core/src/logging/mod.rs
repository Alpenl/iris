//! Rotating diagnostics, panic capture, privacy redaction, and structured JSONL events.
//!
//! Text logs write to `iris.log`; structured events write solely to `events.jsonl`
//! (no dual-write into `iris.log`).

mod event;
mod redact;
mod writer;

use std::backtrace::Backtrace;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::{Arc, Once, OnceLock};

use serde_json::{Value, json};

pub use event::{EVENT_SCHEMA_VERSION, EventLevel, TelemetryEvent, TelemetrySink};
pub use redact::{redact_json, redact_text, url_host};

use event::global_bus;
use writer::{DEFAULT_MAX_BACKUPS, DEFAULT_MAX_FILE_BYTES, RotatingFile};

static TEXT_LOG: OnceLock<Arc<RotatingFile>> = OnceLock::new();

fn should_flush_level(level: log::Level) -> bool {
    matches!(level, log::Level::Warn | log::Level::Error)
}

fn format_log_line(
    timestamp: impl std::fmt::Display,
    level: log::Level,
    target: &str,
    args: std::fmt::Arguments<'_>,
) -> String {
    let body = redact_text(&args.to_string());
    format!("[{timestamp} {level:<5} {target}] {body}\n")
}

/// Ephemeral random 128-bit hex identifier for the current process session.
pub fn session_id() -> &'static str {
    global_bus().session_id()
}

/// Registers an additional telemetry sink (e.g. an integration test collector).
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

        let text_file = if let Some(path) = log_path() {
            match RotatingFile::open(path.clone(), DEFAULT_MAX_FILE_BYTES, DEFAULT_MAX_BACKUPS) {
                Ok(rotating) => {
                    let rotating = Arc::new(rotating);
                    let marker = format!("--- session_start {} ---\n", session_id());
                    let _ = rotating.write_bytes(marker.as_bytes(), true);
                    let _ = TEXT_LOG.set(rotating.clone());
                    Some(rotating)
                }
                Err(e) => {
                    eprintln!("Iris: cannot open log {}: {e}", path.display());
                    None
                }
            }
        } else {
            None
        };
        let text_log_ready = text_file.is_some();

        builder.format(move |buf, record| {
            let line = format_log_line(
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                *record.args(),
            );
            if let Some(file) = &text_file {
                let _ = file.write_bytes(line.as_bytes(), should_flush_level(record.level()));
            }
            buf.write_all(line.as_bytes())
        });

        let events_ready = if let Some(events_path) = events_log_path() {
            match RotatingFile::open(
                events_path.clone(),
                DEFAULT_MAX_FILE_BYTES,
                DEFAULT_MAX_BACKUPS,
            ) {
                Ok(rotating) => {
                    global_bus().set_jsonl_file(Arc::new(rotating));
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

        event(
            EventLevel::Info,
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
    event(
        EventLevel::Info,
        "app",
        "session_end",
        json!({ "uptime_ms": uptime_ms }),
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

/// Emits a structured, automatically redacted event to `events.jsonl` and any registered
/// observer sinks without duplicating the line into `iris.log`.
pub fn event(
    level: EventLevel,
    category: &'static str,
    event: &'static str,
    fields: Value,
) -> TelemetryEvent {
    global_bus().emit(level, category, event, fields)
}

/// Convenience structured-event helper for callers without a direct `serde_json` dependency.
pub fn event_kv(
    level: EventLevel,
    category: &'static str,
    event_name: &'static str,
    fields: &[(&str, &str)],
) -> TelemetryEvent {
    let mut map = serde_json::Map::with_capacity(fields.len());
    for &(k, v) in fields {
        map.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    event(level, category, event_name, Value::Object(map))
}

fn format_panic_payload(info: &PanicHookInfo<'_>) -> (String, String, String) {
    let thread = redact_text(std::thread::current().name().unwrap_or("unnamed"));
    let location = info.location().map_or_else(
        || "<unknown>".to_owned(),
        |loc| redact_text(&format!("{}:{}:{}", loc.file(), loc.line(), loc.column())),
    );
    let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
        redact_text(s)
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        redact_text(s)
    } else {
        "non-string panic payload".to_owned()
    };
    (thread, location, message)
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
        // Fields are already redacted once above; skip redundant re-redaction passes.
        let emitted = global_bus().emit_sanitized(
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
        let line = format!(
            "[panic ts={} ERROR iris::panic] panic on thread '{thread}' at {location}: {message}\n{backtrace}\n",
            emitted.ts_ms
        );
        if let Some(text) = TEXT_LOG.get() {
            let _ = text.write_bytes(line.as_bytes(), true);
        }
        eprint!("{line}");
        flush();
        prev(info);
    }));
}

#[cfg(test)]
mod tests;
