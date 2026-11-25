use super::{ErrorCode, IoOutcome, Transaction};
use mio::{Events, Poll};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use url::Url;

pub(super) fn query(url: &Url) -> Result<BTreeMap<String, String>, ErrorCode> {
    if url.query().is_none_or(|q| q.len() > 8192) {
        return Err(ErrorCode::InvalidArgument);
    }
    let raw = url.query().ok_or(ErrorCode::InvalidArgument)?;
    for (i, byte) in raw.bytes().enumerate() {
        if byte == b'%'
            && raw
                .as_bytes()
                .get(i + 1..i + 3)
                .is_none_or(|s| !s.iter().all(u8::is_ascii_hexdigit))
        {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| ErrorCode::InvalidArgument)?;
    let mut result = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if result.len() >= 32
            || key.len() > 128
            || value.len() > 4096
            || result
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(result)
}
pub(super) fn listen(
    listener: mio::net::TcpListener,
    mut poll: Poll,
    transaction: &Transaction,
) -> Result<IoOutcome, ErrorCode> {
    let mut events = Events::with_capacity(4);
    let mut invalid = 0;
    loop {
        if transaction.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let left = transaction
            .expires
            .saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorCode::Timeout);
        }
        match poll.poll(&mut events, Some(left)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ErrorCode::IoError),
        }
        while let Ok((stream, address)) = listener.accept() {
            if !address.ip().is_loopback() {
                continue;
            }
            let mut stream: TcpStream = stream.into();
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            let Ok(_registration) = transaction.cancel.register(&stream) else {
                if transaction.cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
                continue;
            };
            let outcome = receive(&mut stream, transaction);
            if transaction.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            let accepted = outcome.is_ok();
            let body = if accepted {
                "Authentication received. You may close this window."
            } else {
                "Invalid authentication callback."
            };
            let status = if accepted {
                "200 OK"
            } else {
                "400 Bad Request"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = stream.write_all(response.as_bytes());
            if accepted {
                return outcome;
            }
            invalid += 1;
            if invalid >= 32 {
                return Err(ErrorCode::QuotaExceeded);
            }
        }
    }
}
fn receive(stream: &mut TcpStream, t: &Transaction) -> Result<IoOutcome, ErrorCode> {
    const HEADER_CAP: usize = 32768;
    let end = t.expires.min(Instant::now() + Duration::from_secs(2));
    let mut bytes = Vec::with_capacity(8192);
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ErrorCode::Timeout);
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| ErrorCode::IoError)?;
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).map_err(|_| ErrorCode::IoError)?;
        if n == 0 || bytes.len() + n > HEADER_CAP {
            return Err(ErrorCode::InvalidArgument);
        }
        bytes.extend_from_slice(&buf[..n]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(request.parse(&bytes),Ok(httparse::Status::Complete(n))if n==bytes.len())
        || request.method != Some("GET")
        || request.version != Some(1)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let host = format!(
        "127.0.0.1:{}",
        t.redirect.port().ok_or(ErrorCode::Internal)?
    );
    let hosts: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1
        || hosts[0].value != host.as_bytes()
        || request.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("transfer-encoding")
                || h.name.eq_ignore_ascii_case("content-length")
        })
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let path = request.path.ok_or(ErrorCode::InvalidArgument)?;
    if !path.starts_with('/') || path.starts_with("//") || path.contains(['#', '\\']) {
        return Err(ErrorCode::InvalidArgument);
    }
    let callback = t
        .redirect
        .join(path)
        .map_err(|_| ErrorCode::InvalidArgument)?;
    if path.split('?').next() != Some(t.redirect.path()) || callback.origin() != t.redirect.origin()
    {
        return Err(ErrorCode::Denied);
    }
    let values = query(&callback)?;
    let state = values.get("state").ok_or(ErrorCode::Denied)?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, t.state.as_bytes());
    let expected = ring::hmac::sign(&key, t.state.as_bytes());
    ring::hmac::verify(&key, state.as_bytes(), expected.as_ref()).map_err(|_| ErrorCode::Denied)?;
    if !t.opened.load(Ordering::Acquire) || t.finished.load(Ordering::Acquire) {
        return Err(ErrorCode::BadState);
    }
    if let Some(issuer) = &t.issuer
        && values.get("iss") != Some(issuer)
    {
        return Err(ErrorCode::Denied);
    }
    match (values.get("code"), values.get("error")) {
        (Some(code), None) if !code.is_empty() => Ok(IoOutcome::new(
            serde_json::json!({"code":code,"code_verifier":t.verifier,"redirect_uri":t.redirect.as_str(),"issuer":t.issuer}),
            None,
        )),
        (None, Some(error)) if !error.is_empty() && error.len() <= 256 => Ok(IoOutcome::new(
            serde_json::json!({"oauth_error":error}),
            None,
        )),
        _ => Err(ErrorCode::InvalidArgument),
    }
}
