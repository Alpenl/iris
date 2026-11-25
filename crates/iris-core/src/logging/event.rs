//! Versioned structured diagnostic events and pluggable local/future-telemetry sink abstraction.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::Value;

use super::redact::redact_json;
use super::writer::RotatingFile;

/// Wire schema version for structured diagnostic & future telemetry events.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl EventLevel {
    pub(super) fn to_log_level(self) -> log::Level {
        match self {
            Self::Debug => log::Level::Debug,
            Self::Info => log::Level::Info,
            Self::Warn => log::Level::Warn,
            Self::Error => log::Level::Error,
        }
    }
}

/// Canonical, privacy-sanitized event envelope written to `events.jsonl` and
/// dispatched to any registered [`TelemetrySink`].
#[derive(Clone, Debug, Serialize)]
pub struct TelemetryEvent {
    pub schema: u32,
    pub ts_ms: u64,
    pub uptime_ms: u64,
    pub session_id: String,
    pub seq: u64,
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub level: EventLevel,
    pub category: &'static str,
    pub event: &'static str,
    pub fields: Value,
}

/// Extension point for local exporters or future opt-in telemetry uploaders.
/// Implementations must be non-panicking and thread-safe.
pub trait TelemetrySink: Send + Sync {
    fn record(&self, event: &TelemetryEvent);
    fn flush(&self) {}
}

pub(super) struct JsonlFileSink {
    file: Arc<RotatingFile>,
}

impl JsonlFileSink {
    pub(super) fn new(file: Arc<RotatingFile>) -> Self {
        Self { file }
    }
}

impl TelemetrySink for JsonlFileSink {
    fn record(&self, event: &TelemetryEvent) {
        if let Ok(mut line) = serde_json::to_string(event) {
            line.push('\n');
            let flush = matches!(event.level, EventLevel::Warn | EventLevel::Error);
            let _ = self.file.write_bytes(line.as_bytes(), flush);
        }
    }

    fn flush(&self) {
        self.file.flush();
    }
}

pub(super) struct EventBus {
    session_id: String,
    started_at: Instant,
    seq: AtomicU64,
    sinks: Mutex<Vec<Arc<dyn TelemetrySink>>>,
}

impl EventBus {
    pub(super) fn new() -> Self {
        Self {
            session_id: generate_session_id(),
            started_at: Instant::now(),
            seq: AtomicU64::new(0),
            sinks: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(super) fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub(super) fn register_sink(&self, sink: Arc<dyn TelemetrySink>) {
        self.sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sink);
    }

    pub(super) fn emit(
        &self,
        level: EventLevel,
        category: &'static str,
        event: &'static str,
        fields: Value,
    ) -> TelemetryEvent {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let payload = TelemetryEvent {
            schema: EVENT_SCHEMA_VERSION,
            ts_ms,
            uptime_ms: self.uptime_ms(),
            session_id: self.session_id.clone(),
            seq,
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            level,
            category,
            event,
            fields: redact_json(fields),
        };
        let sinks: Vec<Arc<dyn TelemetrySink>> =
            self.sinks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for sink in sinks {
            sink.record(&payload);
        }
        payload
    }

    pub(super) fn flush(&self) {
        let sinks: Vec<Arc<dyn TelemetrySink>> =
            self.sinks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for sink in sinks {
            sink.flush();
        }
    }
}

pub(super) fn global_bus() -> &'static EventBus {
    static BUS: OnceLock<EventBus> = OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

fn generate_session_id() -> String {
    let mut raw = [0u8; 16];
    if SystemRandom::new().fill(&mut raw).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0u128, |d| d.as_nanos());
        raw = nanos.to_be_bytes();
    }
    let mut hex = String::with_capacity(32);
    for b in raw {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}
