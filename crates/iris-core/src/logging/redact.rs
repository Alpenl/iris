//! Privacy-first PII, path, and credential redaction for logs and structured telemetry events.

use serde_json::Value;
use url::Url;

const REDACTED: &str = "<redacted>";

const SENSITIVE_KEYS: &[&str] = &[
    // Session cookie names of the in-repo `examples/plugins/bilibili` source. They can
    // appear without a `Cookie:` prefix, and plugins cannot yet declare their own
    // secret names to the host, so the host keeps them here.
    "sessdata",
    "bili_jct",
    "dedeuserid",
    "buvid3",
    "buvid4",
    "proxy-authorization",
    "authorization",
    "x-api-key",
    "set-cookie",
    "cookie",
    "access_token",
    "refresh_token",
    "id_token",
    "csrf_token",
    "oauth_token",
    "oauth_signature",
    "client_secret",
    "access_key",
    "access-key",
    "auth_key",
    "auth-key",
    "token",
    "password",
    "passwd",
    "secret",
    "signature",
    "sign",
    "sig",
    "api_key",
    "api-key",
    "apikey",
    "key",
];

fn is_sensitive_key(lower: &str) -> bool {
    let trimmed = lower.trim_matches(['_', '-']);
    !trimmed.is_empty()
        && (SENSITIVE_KEYS.contains(&lower)
            || SENSITIVE_KEYS.contains(&trimmed)
            || trimmed.contains("cookie")
            || trimmed.contains("token")
            || trimmed.contains("csrf")
            || trimmed.contains("xsrf")
            || trimmed.contains("sessionid")
            || trimmed.contains("sessid")
            || trimmed.contains("secret")
            || trimmed.contains("password")
            || trimmed.contains("passwd")
            || trimmed.contains("apikey")
            || trimmed.contains("api_key")
            || trimmed.contains("api-key"))
}

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
pub(super) fn redact_url(raw: &str) -> String {
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
                    if is_sensitive_key(&lower) {
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
    'scan: while i < bytes.len() {
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
                continue 'scan;
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
                    continue 'scan;
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

fn update_unclosed_delimiters(
    segment: &str,
    parens: &mut usize,
    brackets: &mut usize,
    braces: &mut usize,
) {
    for b in segment.bytes() {
        match b {
            b'(' => *parens += 1,
            b')' => *parens = parens.saturating_sub(1),
            b'[' => *brackets += 1,
            b']' => *brackets = brackets.saturating_sub(1),
            b'{' => *braces += 1,
            b'}' => *braces = braces.saturating_sub(1),
            _ => {}
        }
    }
}

fn url_token_end(
    candidate: &str,
    outer_parens: &mut usize,
    outer_brackets: &mut usize,
    outer_braces: &mut usize,
    mut enclosed_single_quote: bool,
) -> usize {
    let mut end = candidate
        .find(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\'))
        .unwrap_or(candidate.len());
    let (mut p_open, mut p_close) = (0usize, 0usize);
    let (mut b_open, mut b_close) = (0usize, 0usize);
    let (mut c_open, mut c_close) = (0usize, 0usize);
    for byte in candidate[..end].bytes() {
        match byte {
            b'(' => p_open += 1,
            b')' => p_close += 1,
            b'[' => b_open += 1,
            b']' => b_close += 1,
            b'{' => c_open += 1,
            b'}' => c_close += 1,
            _ => {}
        }
    }
    let bytes = candidate.as_bytes();
    while end > 0 {
        match bytes[end - 1] {
            b')' if p_close > p_open && *outer_parens > 0 => {
                p_close -= 1;
                *outer_parens -= 1;
                end -= 1;
            }
            b']' if b_close > b_open && *outer_brackets > 0 => {
                b_close -= 1;
                *outer_brackets -= 1;
                end -= 1;
            }
            b'}' if c_close > c_open && *outer_braces > 0 => {
                c_close -= 1;
                *outer_braces -= 1;
                end -= 1;
            }
            b'\'' if enclosed_single_quote => {
                enclosed_single_quote = false;
                end -= 1;
            }
            _ => break,
        }
    }
    end
}

fn redact_embedded_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let (mut parens, mut brackets, mut braces) = (0usize, 0usize, 0usize);
    while let Some(pos) = find_url_scheme(rest) {
        let before = &rest[..pos];
        update_unclosed_delimiters(before, &mut parens, &mut brackets, &mut braces);
        let enclosed_single_quote = before.as_bytes().last() == Some(&b'\'');
        out.push_str(before);
        let candidate = &rest[pos..];
        let end = url_token_end(
            candidate,
            &mut parens,
            &mut brackets,
            &mut braces,
            enclosed_single_quote,
        );
        let raw_url = &candidate[..end];
        out.push_str(&redact_url(raw_url));
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

fn find_url_scheme(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 2;
    while i + 3 <= b.len() {
        if b[i] == b':' && b[i + 1] == b'/' && b[i + 2] == b'/' {
            for scheme in [&b"https"[..], b"http", b"wss", b"ws"] {
                let len = scheme.len();
                if i >= len && b[i - len..i].eq_ignore_ascii_case(scheme) {
                    return Some(i - len);
                }
            }
        }
        i += 1;
    }
    None
}

enum RedactMode {
    Token,
    Quoted { quote: u8, slashes: usize },
    FullLine,
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn skip_hspace(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    pos
}

fn match_auth_scheme(lower: &str, bytes: &[u8], pos: usize) -> Option<usize> {
    for scheme in ["bearer ", "basic "] {
        if lower[pos..].starts_with(scheme) {
            return Some(skip_hspace(bytes, pos + scheme.len()));
        }
    }
    None
}

fn preceding_quote(bytes: &[u8], pos: usize) -> Option<(u8, usize)> {
    let &q @ (b'"' | b'\'') = bytes.get(pos.checked_sub(1)?)? else {
        return None;
    };
    let mut slashes = 0;
    while pos >= 2 + slashes && bytes[pos - 2 - slashes] == b'\\' {
        slashes += 1;
    }
    (slashes + 1).is_power_of_two().then_some((q, slashes))
}

fn consume_matching_quote(bytes: &[u8], pos: usize, quote: u8, slashes: usize) -> Option<usize> {
    if pos + slashes >= bytes.len() {
        return None;
    }
    if bytes[pos..pos + slashes].iter().all(|&b| b == b'\\') && bytes[pos + slashes] == quote {
        Some(pos + slashes + 1)
    } else {
        None
    }
}

fn consume_any_open_quote(bytes: &[u8], pos: usize) -> Option<(usize, u8, usize)> {
    let mut slashes = 0;
    while bytes.get(pos + slashes) == Some(&b'\\') {
        slashes += 1;
    }
    let &q @ (b'"' | b'\'') = bytes.get(pos + slashes)? else {
        return None;
    };
    (slashes + 1)
        .is_power_of_two()
        .then_some((pos + slashes + 1, q, slashes))
}

fn scan_quoted_value(bytes: &[u8], mut pos: usize, quote: u8, slashes: usize) -> (usize, usize) {
    let step = slashes + 1;
    while pos < bytes.len() {
        if matches!(bytes[pos], b'\r' | b'\n') {
            return (pos, pos);
        }
        if bytes[pos] == b'\\' {
            let run_start = pos;
            while pos < bytes.len() && bytes[pos] == b'\\' {
                pos += 1;
            }
            if pos < bytes.len() && bytes[pos] == quote {
                let run = pos - run_start;
                if run < slashes {
                    let delim_start = pos - run;
                    return (delim_start, delim_start);
                }
                let inner_slashes = run - slashes;
                if (inner_slashes / step).is_multiple_of(2) {
                    let delim_start = pos - slashes;
                    return (delim_start, pos + 1);
                }
                pos += 1;
            }
            continue;
        }
        if bytes[pos] == quote {
            if slashes == 0 {
                return (pos, pos + 1);
            }
            return (pos, pos);
        }
        pos += 1;
    }
    (pos, pos)
}

fn redact_secret_assignments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    let lower = input.to_ascii_lowercase();
    while i < bytes.len() {
        let at_word_boundary = i == 0 || !is_key_char(bytes[i - 1]);
        let mut matched: Option<(usize, RedactMode)> = None;
        if at_word_boundary && let Some(val_start) = match_auth_scheme(&lower, bytes, i) {
            let (val_start, mode) = match consume_any_open_quote(bytes, val_start) {
                Some((after_q, quote, slashes)) => (after_q, RedactMode::Quoted { quote, slashes }),
                None => (val_start, RedactMode::Token),
            };
            matched = Some((val_start, mode));
        }
        if matched.is_none() && at_word_boundary && is_key_char(bytes[i]) {
            let open_quote = preceding_quote(bytes, i);
            let mut key_end = i;
            while key_end < bytes.len() && is_key_char(bytes[key_end]) {
                key_end += 1;
            }
            let key = &lower[i..key_end];
            if is_sensitive_key(key) {
                let mut pos = key_end;
                let key_quoted = if let Some((q, slashes)) = open_quote
                    && let Some(after_q) = consume_matching_quote(bytes, pos, q, slashes)
                {
                    pos = after_q;
                    true
                } else {
                    false
                };
                if open_quote.is_none() || key_quoted {
                    let after_ws = skip_hspace(bytes, pos);
                    if let Some(&sep @ (b'=' | b':')) = bytes.get(after_ws) {
                        let mut val_start = skip_hspace(bytes, after_ws + 1);
                        if let Some(after_scheme) = match_auth_scheme(&lower, bytes, val_start) {
                            val_start = after_scheme;
                        }
                        let mode = if let Some((after_q, quote, slashes)) =
                            consume_any_open_quote(bytes, val_start)
                        {
                            val_start = after_q;
                            RedactMode::Quoted { quote, slashes }
                        } else if !key_quoted
                            && sep == b':'
                            && matches!(
                                key,
                                "cookie" | "set-cookie" | "authorization" | "proxy-authorization"
                            )
                        {
                            RedactMode::FullLine
                        } else {
                            RedactMode::Token
                        };
                        matched = Some((val_start, mode));
                    }
                }
            }
        }
        if let Some((val_start, mode)) = matched {
            out.push_str(&input[i..val_start]);
            out.push_str(REDACTED);
            match mode {
                RedactMode::Quoted { quote, slashes } => {
                    let (val_end, close_end) = scan_quoted_value(bytes, val_start, quote, slashes);
                    out.push_str(&input[val_end..close_end]);
                    i = close_end;
                }
                RedactMode::FullLine => {
                    let mut end = val_start;
                    while end < bytes.len() && !matches!(bytes[end], b'\r' | b'\n') {
                        end += 1;
                    }
                    i = end;
                }
                RedactMode::Token => {
                    let mut end = val_start;
                    while end < bytes.len()
                        && !matches!(
                            bytes[end],
                            b'&' | b';'
                                | b','
                                | b'\r'
                                | b'\n'
                                | b'"'
                                | b'\''
                                | b'\\'
                                | b' '
                                | b'\t'
                                | b')'
                                | b']'
                                | b'}'
                        )
                    {
                        end += 1;
                    }
                    i = end;
                }
            }
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}
