use super::event::EventBus;
use super::redact::redact_url;
use super::writer::{RotatingFile, backup_path};
use super::{
    EVENT_SCHEMA_VERSION, EventLevel, TelemetryEvent, TelemetrySink, format_log_line, redact_json,
    redact_text, register_sensitive_keys, should_flush_level, url_host,
};
use crate::test_util::TestDir;
use crate::vault::CredentialVault;
use serde_json::{Value, json};
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
fn rotating_file_rotates_on_size_limit_and_bounds_backups() {
    let dir = TestDir::new("log-rotate");
    let path = dir.join("iris.log");
    {
        let first = RotatingFile::open(path.clone(), 1024, 3).unwrap();
        first.write_bytes(&vec![b'a'; 1024], true).unwrap();
    }
    // Reopening a file already at max_bytes rotates it into iris.1.log.
    let second = RotatingFile::open(path.clone(), 1024, 3).unwrap();
    assert_eq!(std::fs::read(backup_path(&path, 1)).unwrap().len(), 1024);
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

    // R1-H03 regressions: spaces after `=`, quoted values, JSON in text, full-line Cookie/Set-Cookie,
    // and adjacent home-path / secret after `/Users/<name>/`.
    assert_eq!(redact_text("token= secret"), "token= <redacted>");
    assert_eq!(redact_text(r#"token="secret""#), r#"token="<redacted>""#);
    assert_eq!(redact_text("password='my secret'"), "password='<redacted>'");
    assert_eq!(
        redact_text(r#"{"token": "secret", "host": "ok"}"#),
        r#"{"token": "<redacted>", "host": "ok"}"#
    );
    assert_eq!(
        redact_text(r#"{"cookie": "a=1; b=2", "host": "ok"}"#),
        r#"{"cookie": "<redacted>", "host": "ok"}"#
    );
    assert_eq!(redact_text("Cookie: a=1; b=2"), "Cookie: <redacted>");
    assert_eq!(
        redact_text("Set-Cookie: sid=1; HttpOnly\nnext=ok"),
        "Set-Cookie: <redacted>\nnext=ok"
    );
    assert_eq!(
        redact_text("Authorization: Token ghp_secret_pat_123"),
        "Authorization: <redacted>"
    );
    assert_eq!(
        redact_text(r#"Authorization: Digest username="alice", response="deadbeef""#),
        "Authorization: <redacted>"
    );
    assert_eq!(
        redact_text("Authorization: OAuth oauth_token=sec_1, oauth_signature=sec_2"),
        "Authorization: <redacted>"
    );
    assert_eq!(
        redact_text("Proxy-Authorization: NTLM TlRMTVNTUAABAAA\r\nHost: example.com"),
        "Proxy-Authorization: <redacted>\r\nHost: example.com"
    );
    assert_eq!(
        redact_text(r"/Users/alice/C:\Users\bob\video.mp4"),
        r"<USER_HOME>/<USER_HOME>\video.mp4"
    );
    assert_eq!(
        redact_text("/Users/alice/token=secret"),
        "<USER_HOME>/token=<redacted>"
    );

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
fn redaction_covers_generic_csrf_and_session_id_names() {
    // Bilibili-style form bodies carry the `bili_jct` value as a bare `csrf` field.
    assert_eq!(
        redact_text("aid=1&csrf=999&ok=1"),
        "aid=1&csrf=<redacted>&ok=1"
    );
    assert_eq!(redact_text("xsrf=abc"), "xsrf=<redacted>");
    assert_eq!(
        redact_text("PHPSESSID=abc; JSESSIONID=def; ASP.NET_SessionId=ghi"),
        "PHPSESSID=<redacted>; JSESSIONID=<redacted>; ASP.NET_SessionId=<redacted>"
    );
    // Numeric stream session ids in structured events stay readable.
    let payload = redact_json(json!({"session": 5, "csrf": "999", "X-Csrf": "1"}));
    assert_eq!(payload["session"], 5);
    assert_eq!(payload["csrf"], "<redacted>");
    assert_eq!(payload["X-Csrf"], "<redacted>");
}

#[test]
fn redaction_covers_ipv6_urls_delimited_userinfo_escaped_json_and_hyphen_underscore_keys() {
    // (a) IPv6 bracketed literals, balanced/unbalanced path delimiters, and Basic Auth userinfo.
    assert_eq!(
        redact_text("fetch http://[2001:db8::1]:8080/play?ticket=secret_v6 failed"),
        "fetch http://[2001:db8::1]:8080/<redacted> failed"
    );
    assert_eq!(
        redact_text("[url=http://[2001:db8::1]:8080/play?ticket=secret_v6]"),
        "[url=http://[2001:db8::1]:8080/<redacted>]"
    );
    assert_eq!(
        redact_text("(https://cdn.example/Movie_(2026)_[1080p]/it's_ep1.mkv?ticket=secret_path)"),
        "(https://cdn.example/<redacted>)"
    );
    let basic_auth = redact_text(
        "dav error ('https://admin:pa)ss]w'0rd@nas.example:8443/dav/file.mkv?ticket=sec')",
    );
    assert!(!basic_auth.contains("pa)"), "{basic_auth}");
    assert!(!basic_auth.contains("ss]"), "{basic_auth}");
    assert!(!basic_auth.contains("w'0rd"), "{basic_auth}");
    assert!(!basic_auth.contains("ticket=sec"), "{basic_auth}");
    assert_eq!(
        basic_auth,
        "dav error ('https://nas.example:8443/<redacted>')"
    );

    // (b) Debug-formatted / nested JSON strings with escaped quotes `\"` and inner `\\\"`.
    let nested = format!(
        "{:?}",
        r#"{"token": "secret_123", "quote_secret": "sec\"ret", "host": "ok"}"#
    );
    let redacted_nested = redact_text(&nested);
    assert!(!redacted_nested.contains("secret_123"), "{redacted_nested}");
    assert!(!redacted_nested.contains("sec\\"), "{redacted_nested}");
    assert!(
        redacted_nested.contains(r#"\"host\": \"ok\""#),
        "{redacted_nested}"
    );
    assert_eq!(
        redacted_nested,
        r#""{\"token\": \"<redacted>\", \"quote_secret\": \"<redacted>\", \"host\": \"ok\"}""#
    );

    // (c) & (d) Hyphenated (`x-api-key`) and underscore-containing (`client_secret`, `csrf_token`, `id_token`) keys.
    assert_eq!(
        redact_text(r#"{"x-api-key": "k_live_secret", "host": "api.example"}"#),
        r#"{"x-api-key": "<redacted>", "host": "api.example"}"#
    );
    assert_eq!(
        redact_text("X-Api-Key: k_live_secret\nHost: api.example"),
        "X-Api-Key: <redacted>\nHost: api.example"
    );
    assert_eq!(
        redact_text("client_secret=cs_999&csrf_token=csrf_888&id_token=eyJ_777&monkey=ok"),
        "client_secret=<redacted>&csrf_token=<redacted>&id_token=<redacted>&monkey=ok"
    );
    let json_obj = redact_json(json!({
        "X-Api-Key": "k_live_secret",
        "client_secret": "cs_999",
        "csrf_token": "csrf_888",
        "id_token": "eyJ_777",
        "debug_body": r#"{"x-api-key": "nested_key", "id_token": "nested_jwt"}"#,
        "host": "api.example"
    }));
    let dumped = json_obj.to_string();
    for secret in [
        "k_live_secret",
        "cs_999",
        "csrf_888",
        "eyJ_777",
        "nested_key",
        "nested_jwt",
    ] {
        assert!(!dumped.contains(secret), "leaked {secret}: {dumped}");
    }
    assert_eq!(json_obj["host"], "api.example");
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
    let rotating = Arc::new(RotatingFile::open(events_path.clone(), 4096, 2).unwrap());
    let bus = EventBus::new();
    bus.set_jsonl_file(rotating);
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
    let rotating = Arc::new(RotatingFile::open(events_path.clone(), 64 * 1024, 2).unwrap());
    let bus = Arc::new(EventBus::new());
    bus.set_jsonl_file(rotating);

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

#[test]
fn plugin_declared_sensitive_keys_and_vault_secret_values_redact_without_hardcoded_host_names() {
    // 1. Plugin-specific key names are declared via manifest/registration rather than hardcoded in host.
    assert_eq!(
        redact_text("custom_plugin_sid=bili_live_val_42"),
        "custom_plugin_sid=bili_live_val_42"
    );
    register_sensitive_keys(["custom_plugin_sid", "sessdata", "bili_jct"]);
    assert_eq!(
        redact_text("custom_plugin_sid=bili_live_val_42; SESSDATA=sec_99; bili_jct=csrf_11"),
        "custom_plugin_sid=<redacted>; SESSDATA=<redacted>; bili_jct=<redacted>"
    );
    let json_val = redact_json(json!({
        "Custom_Plugin_Sid": "bili_live_val_42",
        "ok_field": "visible"
    }));
    assert_eq!(json_val["Custom_Plugin_Sid"], "<redacted>");
    assert_eq!(json_val["ok_field"], "visible");

    // 2. CredentialVault automatically registers secret values so bare/keyless leaks are scrubbed.
    let vault = CredentialVault::in_memory();
    let opaque_secret = "opaque-vault-credential-value-987654321";
    vault
        .set_secret("plugin.custom", "arbitrary_slot", Some(opaque_secret))
        .unwrap();
    let scrubbed_text = redact_text(&format!(
        "upstream rejected header {opaque_secret} on retry"
    ));
    assert!(!scrubbed_text.contains(opaque_secret), "{scrubbed_text}");
    assert_eq!(
        scrubbed_text,
        "upstream rejected header <redacted> on retry"
    );

    let scrubbed_json = redact_json(json!({
        "unrecognized_field": format!("raw={opaque_secret}"),
        "safe": "unchanged"
    }));
    assert!(!scrubbed_json.to_string().contains(opaque_secret));
    assert_eq!(scrubbed_json["unrecognized_field"], "raw=<redacted>");
    assert_eq!(scrubbed_json["safe"], "unchanged");
}
