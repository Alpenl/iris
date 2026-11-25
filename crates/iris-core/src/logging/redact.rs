//! Privacy-first PII, path, and credential redaction for logs and structured telemetry events.

use serde_json::Value;
use url::Url;

const REDACTED: &str = "<redacted>";

const SENSITIVE_KEYS: &[&str] = &[
    "sessdata",
    "bili_jct",
    "dedeuserid",
    "buvid3",
    "buvid4",
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "access_token",
    "refresh_token",
    "access_key",
    "auth_key",
    "token",
    "password",
    "passwd",
    "secret",
    "sig",
    "sign",
    "signature",
    "api_key",
    "apikey",
    "key",
];

/// Extracts only `host` (and non-default port if present) from a URL, never exposing
/// userinfo, paths, or query parameters.
pub fn url_host(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => "<unknown-host>".to_owned(),
        },
        Err(_) => "<invalid-url>".to_owned(),
    }
}

/// Redacts credentials, paths beyond origin, query strings, and fragments from a URL.
pub fn redact_url(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(url) => {
            let scheme = url.scheme();
            match (url.host_str(), url.port()) {
                (Some(host), Some(port)) => format!("{scheme}://{host}:{port}/{REDACTED}"),
                (Some(host), None) => format!("{scheme}://{host}/{REDACTED}"),
                _ => format!("{scheme}://{REDACTED}"),
            }
        }
        Err(_) => REDACTED.to_owned(),
    }
}

/// Sanitizes arbitrary diagnostic text by removing user home directory names,
/// embedded URL credentials/queries, and secret key-value pairs.
pub fn redact_text(raw: &str) -> String {
    let without_homes = redact_home_paths(raw);
    let without_urls = redact_embedded_urls(&without_homes);
    redact_secret_assignments(&without_urls)
}

/// Recursively sanitizes all strings and sensitive field keys inside structured JSON payloads.
pub fn redact_json(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_text(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let lower = k.to_ascii_lowercase();
                    if SENSITIVE_KEYS.iter().any(|s| *s == lower)
                        || lower.contains("cookie")
                        || lower.contains("token")
                        || lower.contains("secret")
                        || lower.contains("password")
                    {
                        (k, Value::String(REDACTED.to_owned()))
                    } else {
                        (k, redact_json(v))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

fn redact_home_paths(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Match Windows drive user profiles: `C:\Users\<name>\` or `C:/Users/<name>/`
        if i + 9 < bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && matches!(bytes[i + 2], b'\\' | b'/')
            && bytes[i + 3..].len() >= 6
            && bytes[i + 3..i + 8].eq_ignore_ascii_case(b"users")
            && matches!(bytes[i + 8], b'\\' | b'/')
        {
            let sep = bytes[i + 8];
            let mut end = i + 9;
            while end < bytes.len()
                && bytes[end] != b'\\'
                && bytes[end] != b'/'
                && !bytes[end].is_ascii_whitespace()
                && !matches!(bytes[end], b'"' | b'\'' | b')' | b']' | b',')
            {
                end += 1;
            }
            if end > i + 9 {
                out.push_str("<USER_HOME>");
                if end < bytes.len() && matches!(bytes[end], b'\\' | b'/') {
                    out.push(sep as char);
                    end += 1;
                }
                i = end;
                continue;
            }
        }
        // Match Unix home profiles: `/Users/<name>/` or `/home/<name>/`
        for prefix in [&b"/Users/"[..], &b"/home/"[..]] {
            if bytes[i..].starts_with(prefix) && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
                let mut end = i + prefix.len();
                while end < bytes.len()
                    && bytes[end] != b'/'
                    && !bytes[end].is_ascii_whitespace()
                    && !matches!(bytes[end], b'"' | b'\'' | b')' | b']' | b',')
                {
                    end += 1;
                }
                if end > i + prefix.len() {
                    out.push_str("<USER_HOME>");
                    if end < bytes.len() && bytes[end] == b'/' {
                        out.push('/');
                        end += 1;
                    }
                    i = end;
                    break;
                }
            }
        }
        if i < bytes.len() {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn redact_embedded_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = find_url_scheme(rest) {
        out.push_str(&rest[..pos]);
        let candidate = &rest[pos..];
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']'))
            .unwrap_or(candidate.len());
        let raw_url = &candidate[..end];
        out.push_str(&redact_url(raw_url));
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

fn find_url_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    ["https://", "http://", "wss://", "ws://"]
        .into_iter()
        .filter_map(|scheme| lower.find(scheme))
        .min()
}

fn redact_secret_assignments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let lower = input.to_ascii_lowercase();
    while i < input.len() {
        let at_word_boundary = i == 0
            || !(input.as_bytes()[i - 1].is_ascii_alphanumeric()
                || input.as_bytes()[i - 1] == b'_');
        let mut matched = None;
        if at_word_boundary {
            for scheme in ["bearer ", "basic "] {
                if lower[i..].starts_with(scheme) {
                    let after_scheme = i + scheme.len();
                    let extra_spaces = input[after_scheme..]
                        .bytes()
                        .take_while(|&b| b == b' ')
                        .count();
                    let val_start = after_scheme + extra_spaces;
                    matched = Some((val_start, &input[i..val_start]));
                    break;
                }
            }
        }
        if matched.is_none() && at_word_boundary {
            for key in SENSITIVE_KEYS {
                if lower[i..].starts_with(key) {
                    let after = i + key.len();
                    let tail = &input[after..];
                    let sep_len = if tail.starts_with('=') {
                        Some(1)
                    } else if let Some(after_colon) = tail.strip_prefix(':') {
                        let spaces = after_colon.bytes().take_while(|&b| b == b' ').count();
                        Some(1 + spaces)
                    } else {
                        None
                    };
                    if let Some(sep_len) = sep_len {
                        let mut val_start = after + sep_len;
                        for scheme in ["bearer ", "basic "] {
                            if lower[val_start..].starts_with(scheme) {
                                val_start += scheme.len();
                                val_start += input[val_start..]
                                    .bytes()
                                    .take_while(|&b| b == b' ')
                                    .count();
                                break;
                            }
                        }
                        matched = Some((val_start, &input[i..val_start]));
                        break;
                    }
                }
            }
        }
        if let Some((val_start, prefix)) = matched {
            out.push_str(prefix);
            out.push_str(REDACTED);
            let mut end = val_start;
            let bytes = input.as_bytes();
            while end < bytes.len()
                && !matches!(
                    bytes[end],
                    b'&' | b';' | b',' | b'\r' | b'\n' | b'"' | b'\'' | b' ' | b'\t' | b')' | b']'
                )
            {
                end += 1;
            }
            i = end;
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}
