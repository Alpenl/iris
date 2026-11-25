use super::event::{EventBus, JsonlFileSink};
use super::writer::{RotatingFile, backup_path};
use super::*;
use crate::test_util::TestDir;
use std::sync::{Arc, Mutex};

#[test]
fn format_log_line_redacts_secrets_and_flushes_on_typed_levels() {
    let line = format_log_line(
        "2026-11-26T00:00:00.000Z",
        log::Level::Info,
        "iris_core::settings",
        format_args!(
            r"loaded C:\Users\alice\AppData\Iris\settings.conf from https://cdn.example/a?token=secret"
        ),
    );
    assert!(!line.contains("alice"), "leaked username: {line}");
    assert!(!line.contains("token=secret"), "leaked token: {line}");
    assert!(line.contains("<USER_HOME>"));
    assert!(line.contains("https://cdn.example/<redacted>"));
    assert!(!should_flush_level(log::Level::Info));
    assert!(!should_flush_level(log::Level::Debug));
    assert!(should_flush_level(log::Level::Warn));
    assert!(should_flush_level(log::Level::Error));
}

#[test]
fn rotating_file_preserves_prior_session_and_bounds_backups() {
    let dir = TestDir::new("log-rotate");
    let path = dir.join("iris.log");
    {
        let first = RotatingFile::open(path.clone(), 1024, 3, true).unwrap();
        first.write_bytes(b"session-1-line\n", true).unwrap();
    }
    // Reopening with rotate_existing_on_open preserves prior session as iris.1.log.
    let second = RotatingFile::open(path.clone(), 1024, 3, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(backup_path(&path, 1)).unwrap(),
        "session-1-line\n"
    );
    // Write enough chunks to trigger multiple mid-session rotations.
    let block = vec![b'x'; 600];
    for _ in 0..8 {
        second.write_bytes(&block, true).unwrap();
    }
    assert!(path.exists());
    assert!(backup_path(&path, 1).exists());
    assert!(backup_path(&path, 2).exists());
    assert!(backup_path(&path, 3).exists());
    assert!(!backup_path(&path, 4).exists());
}

#[test]
fn redaction_strips_user_paths_url_tokens_and_secret_fields() {
    assert_eq!(
        url_host("https://user:pass@upos-hz.bilivideo.com:8443/path/video.m4s?sign=secret"),
        "upos-hz.bilivideo.com:8443"
    );
    assert_eq!(
        redact_url("https://upos-hz.bilivideo.com/upgcxcode/video.m4s?auth_key=abc&e=123"),
        "https://upos-hz.bilivideo.com/<redacted>"
    );
    let sample = r"Failed opening C:\Users\alice\Videos\secret.mkv via https://cdn.example/v.m4s?token=xyz with Cookie: SESSDATA=12345; bili_jct=999";
    let clean = redact_text(sample);
    assert!(!clean.contains("alice"), "leaked username: {clean}");
    assert!(!clean.contains("token=xyz"), "leaked query token: {clean}");
    assert!(!clean.contains("12345"), "leaked SESSDATA: {clean}");
    assert!(!clean.contains("999"), "leaked bili_jct: {clean}");
    assert!(clean.contains("<USER_HOME>\\Videos\\secret.mkv"));
    assert!(clean.contains("https://cdn.example/<redacted>"));

    let unix = redact_text("open /Users/bob/Movies/clip.mp4 and /home/charlie/a.mkv");
    assert!(!unix.contains("bob"));
    assert!(!unix.contains("charlie"));
    assert!(unix.contains("<USER_HOME>/Movies/clip.mp4"));
    assert!(unix.contains("<USER_HOME>/a.mkv"));

    let payload = redact_json(json!({
        "host": "upos.bilivideo.com",
        "token": "should-never-appear",
        "nested": {
            "url": "https://cdn.example/a?sig=topsecret",
            "cookie_header": "SESSDATA=raw"
        }
    }));
    let serialized = payload.to_string();
    assert!(!serialized.contains("should-never-appear"));
    assert!(!serialized.contains("topsecret"));
    assert!(!serialized.contains("raw"));
    assert_eq!(payload["host"], "upos.bilivideo.com");
}

#[test]
fn structured_event_bus_writes_valid_redacted_jsonl_and_notifies_custom_sinks() {
    struct MemorySink(Mutex<Vec<TelemetryEvent>>);
    impl TelemetrySink for MemorySink {
        fn record(&self, event: &TelemetryEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    let dir = TestDir::new("log-jsonl");
    let events_path = dir.join("events.jsonl");
    let rotating = Arc::new(RotatingFile::open(events_path.clone(), 4096, 2, false).unwrap());
    let bus = EventBus::new();
    bus.register_sink(Arc::new(JsonlFileSink::new(rotating)));
    let mem = Arc::new(MemorySink(Mutex::new(Vec::new())));
    bus.register_sink(mem.clone());

    let e1 = bus.emit(
        EventLevel::Info,
        "stream",
        "upstream_resumed",
        json!({
            "host": "upos.bilivideo.com",
            "offset": 1024,
            "detail": r"path C:\Users\tester\clip.mp4 https://a.test/x?auth_key=leak",
        }),
    );
    let e2 = bus.emit(
        EventLevel::Warn,
        "player",
        "hwdec_changed",
        json!({ "from": "d3d11va", "to": "no" }),
    );
    bus.flush();

    assert_eq!(e1.schema, EVENT_SCHEMA_VERSION);
    assert_eq!(e1.seq, 1);
    assert_eq!(e2.seq, 2);
    assert_eq!(e1.session_id, e2.session_id);
    assert_eq!(e1.session_id.len(), 32);

    let captured = mem.0.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let raw = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["category"], "stream");
    assert_eq!(lines[0]["event"], "upstream_resumed");
    assert_eq!(lines[0]["fields"]["offset"], 1024);
    assert!(!lines[0].to_string().contains("tester"));
    assert!(!lines[0].to_string().contains("auth_key=leak"));
    assert_eq!(lines[1]["level"], "warn");
    assert_eq!(lines[1]["fields"]["to"], "no");
}

#[test]
fn concurrent_event_writes_produce_clean_uncorrupted_jsonl() {
    let dir = TestDir::new("log-concurrent");
    let events_path = dir.join("events.jsonl");
    let rotating = Arc::new(RotatingFile::open(events_path.clone(), 64 * 1024, 2, false).unwrap());
    let bus = Arc::new(EventBus::new());
    bus.register_sink(Arc::new(JsonlFileSink::new(rotating)));

    let handles: Vec<_> = (0..4)
        .map(|t| {
            let bus = bus.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    bus.emit(
                        EventLevel::Info,
                        "test",
                        "concurrent_tick",
                        json!({ "thread": t, "iter": i }),
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    bus.flush();

    let raw = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<Value> = raw
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 100);
}
