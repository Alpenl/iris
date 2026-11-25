//! Protocol-2 HTTP: exact header pairs, explicit redirect policy, streaming
//! uploads, and request authority retained through every body read.
mod response;

use super::{Origin, RequestGrant, TransportKind, normalize_url};
use crate::vfs::http_client::{
    HttpBody, HttpCancel, HttpClientError, NetworkScope, SocketRegistration, Transport,
    connect_transport, is_tchar,
};
use iris_api::v2::ErrorCode;
use std::{
    io::{self, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};
use url::{Position, Url};

pub(crate) type Headers = Vec<(String, String)>;
pub(crate) trait Cookies: Send + Sync {
    fn header(&self, url: &Url) -> Option<String>;
    fn receive(&self, url: &Url, headers: &Headers) -> Result<(), ErrorCode>;
}

#[derive(Clone)]
pub(crate) struct RawRequest {
    pub method: String,
    pub url: Url,
    pub headers: Headers,
    pub follow_redirects: bool,
    pub timeout: Duration,
    pub cookies: Option<Arc<dyn Cookies>>,
}

struct Connection {
    transport: Transport,
    registration: SocketRegistration,
    grant: Arc<RequestGrant>,
    cancel: HttpCancel,
}
pub(crate) struct Upload {
    connection: Connection,
    request: RawRequest,
    deadline: Instant,
}
pub(crate) struct Response {
    pub status: u16,
    pub url: Url,
    pub headers: Headers,
    body: HttpBody,
    grant: Arc<RequestGrant>,
    cancel: HttpCancel,
    _registration: SocketRegistration,
}
pub(crate) enum RawState {
    Upload(Upload),
    Response(Response),
    Closed,
}

pub(crate) fn validate_headers(headers: &Headers) -> Result<(), ErrorCode> {
    let mut total = 0usize;
    if headers.len() > 128 {
        return Err(ErrorCode::QuotaExceeded);
    }
    for (name, value) in headers {
        total = total.saturating_add(name.len() + value.len() + 4);
        if name.is_empty()
            || !name.bytes().all(is_tchar)
            || value.bytes().any(|b| b == 0x7f || (b < 0x20 && b != b'\t'))
            || value.chars().any(|c| u32::from(c) > 255)
        {
            return Err(ErrorCode::InvalidArgument);
        }
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "proxy-authorization"
                | "proxy-connection"
                | "upgrade"
                | "te"
                | "trailer"
                | "expect"
        ) {
            return Err(ErrorCode::Denied);
        }
    }
    if total > 65536 {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(())
}

pub(crate) fn open(
    request: RawRequest,
    body: Option<&[u8]>,
    streaming: bool,
    grant: Arc<RequestGrant>,
    cancel: HttpCancel,
) -> Result<RawState, ErrorCode> {
    validate_headers(&request.headers)?;
    if request.method.is_empty()
        || request.method.len() > 32
        || !request.method.bytes().all(is_tchar)
        || (streaming && body.is_some())
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let deadline = Instant::now() + request.timeout.min(Duration::from_secs(60));
    if streaming {
        let connection = begin(&request, None, true, grant, cancel, deadline)?;
        Ok(RawState::Upload(Upload {
            connection,
            request,
            deadline,
        }))
    } else {
        follow(request, body, grant, cancel, deadline).map(RawState::Response)
    }
}

fn begin(
    request: &RawRequest,
    body: Option<&[u8]>,
    streaming: bool,
    grant: Arc<RequestGrant>,
    cancel: HttpCancel,
    deadline: Instant,
) -> Result<Connection, ErrorCode> {
    grant.check_live()?;
    grant.check_credentials(&request.url, &request.headers)?;
    if cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    let scope = NetworkScope::authorized(request.url.clone(), grant.clone())
        .map_err(|_| ErrorCode::Denied)?;
    let (mut transport, registration) = connect_transport(
        &request.url,
        &scope,
        &cancel,
        Some(deadline.min(Instant::now() + Duration::from_secs(30))),
    )
    .map_err(|error| transport_error(error, &grant, &cancel))?;
    let origin = Origin::of(&request.url)?;
    let host = if origin.host.contains(':') {
        format!("[{}]", origin.host)
    } else {
        origin.host
    };
    let target = &request.url[Position::BeforePath..Position::AfterQuery];
    let default_port = matches!(
        (origin.scheme.as_str(), origin.port),
        ("http", 80) | ("https", 443)
    );
    let host_header = if default_port {
        host
    } else {
        format!("{host}:{}", origin.port)
    };
    let mut wire = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        request.method, target, host_header
    );
    if !request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
    {
        wire.push_str("Accept-Encoding: identity\r\n");
    }
    for (name, value) in &request.headers {
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    if !request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        && let Some(cookie) = request
            .cookies
            .as_ref()
            .and_then(|jar| jar.header(&request.url))
    {
        grant.check_credentials(&request.url, &[("cookie".into(), cookie.clone())])?;
        wire.push_str("Cookie: ");
        wire.push_str(&cookie);
        wire.push_str("\r\n");
    }
    if streaming {
        wire.push_str("Transfer-Encoding: chunked\r\n");
    } else if let Some(body) = body {
        wire.push_str(&format!("Content-Length: {}\r\n", body.len()));
    } else if !matches!(request.method.as_str(), "GET" | "HEAD") {
        wire.push_str("Content-Length: 0\r\n");
    }
    wire.push_str("\r\n");
    if wire.len() > 80 * 1024 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let wire: Vec<u8> = wire.chars().map(|c| c as u8).collect();
    transport
        .write_all(&wire)
        .map_err(|e| io_error(e, &grant, &cancel))?;
    if let Some(body) = body {
        transport
            .write_all(body)
            .map_err(|e| io_error(e, &grant, &cancel))?;
    }
    transport
        .flush()
        .map_err(|e| io_error(e, &grant, &cancel))?;
    Ok(Connection {
        transport,
        registration,
        grant,
        cancel,
    })
}

fn follow(
    mut request: RawRequest,
    mut body: Option<&[u8]>,
    grant: Arc<RequestGrant>,
    cancel: HttpCancel,
    deadline: Instant,
) -> Result<Response, ErrorCode> {
    let mut seen = std::collections::BTreeSet::new();
    for hop in 0..=10 {
        if !seen.insert((request.method.clone(), request.url.as_str().to_owned())) {
            return Err(ErrorCode::InvalidArgument);
        }
        let connection = begin(
            &request,
            body,
            false,
            grant.clone(),
            cancel.clone(),
            deadline,
        )?;
        let response = response::receive(connection, &request, deadline)?;
        if !request.follow_redirects || !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let Some(location) = unique_header(&response.headers, "location")? else {
            return Ok(response);
        };
        if hop == 10 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let next = request
            .url
            .join(location)
            .map_err(|_| ErrorCode::InvalidArgument)?;
        let next = normalize_url(next.as_str(), TransportKind::Http)?;
        grant.check_redirect(&request.url, &next)?;
        let change = Origin::of(&request.url)? != Origin::of(&next)?;
        if response.status == 303 && request.method != "HEAD"
            || matches!(response.status, 301 | 302) && request.method == "POST"
        {
            request.method = "GET".into();
            body = None;
            request
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        }
        if change {
            if body.is_some_and(|b| !b.is_empty()) {
                return Err(ErrorCode::Denied);
            }
            request.cookies = None;
            let mut kept = Vec::with_capacity(request.headers.len());
            for (name, value) in std::mem::take(&mut request.headers) {
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "accept" | "accept-encoding" | "user-agent" | "range"
                ) {
                    kept.push((name, value));
                } else if lower == "referer"
                    && let Some(origin) = cross_origin_referer(&value, &next)
                {
                    kept.push((name, origin));
                }
            }
            request.headers = kept;
        }
        request.url = next;
    }
    Err(ErrorCode::InvalidArgument)
}

impl RawState {
    pub(crate) fn write(&mut self, bytes: &[u8], finish: bool) -> Result<(), ErrorCode> {
        let Self::Upload(upload) = self else {
            return Err(ErrorCode::BadState);
        };
        upload.connection.grant.check_live()?;
        if upload.connection.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if Instant::now() >= upload.deadline {
            return Err(ErrorCode::Timeout);
        }
        let conn = &mut upload.connection;
        if !bytes.is_empty() {
            write!(conn.transport, "{:x}\r\n", bytes.len())
                .and_then(|()| conn.transport.write_all(bytes))
                .and_then(|()| conn.transport.write_all(b"\r\n"))
                .map_err(|e| io_error(e, &conn.grant, &conn.cancel))?;
        } else if !finish {
            return Err(ErrorCode::InvalidArgument);
        }
        if finish {
            conn.transport
                .write_all(b"0\r\n\r\n")
                .map_err(|e| io_error(e, &conn.grant, &conn.cancel))?;
        }
        conn.transport
            .flush()
            .map_err(|e| io_error(e, &conn.grant, &conn.cancel))?;
        if finish {
            let Self::Upload(upload) = std::mem::replace(self, Self::Closed) else {
                return Err(ErrorCode::BadState);
            };
            let response = response::receive(upload.connection, &upload.request, upload.deadline)?;
            if upload.request.follow_redirects
                && matches!(response.status, 301 | 302 | 303 | 307 | 308)
            {
                return Err(ErrorCode::Unsupported);
            }
            *self = Self::Response(response);
        }
        Ok(())
    }

    pub(crate) fn read(&mut self, out: &mut [u8], deadline: Instant) -> Result<usize, ErrorCode> {
        let Self::Response(response) = self else {
            return Err(ErrorCode::BadState);
        };
        response.grant.check_live()?;
        if response.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        response
            .body
            .set_body_deadline(Some(deadline))
            .map_err(|e| io_error(e, &response.grant, &response.cancel))?;
        let n = response
            .body
            .read(out)
            .map_err(|e| io_error(e, &response.grant, &response.cancel))?;
        response.grant.check_live()?;
        if response.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(n)
    }
}

fn cross_origin_referer(value: &str, next: &Url) -> Option<String> {
    let parsed = Url::parse(value.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.scheme() == "https" && next.scheme() != "https")
    {
        return None;
    }
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}/", parsed.scheme()),
        None => format!("{}://{host}/", parsed.scheme()),
    })
}

fn unique_header<'a>(headers: &'a Headers, name: &str) -> Result<Option<&'a str>, ErrorCode> {
    let mut values = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let first = values.next().map(|(_, value)| value.as_str());
    if values.next().is_some() {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(first)
}
fn io_error(error: io::Error, grant: &RequestGrant, cancel: &HttpCancel) -> ErrorCode {
    if let Err(error) = grant.check_live() {
        return error;
    }
    if cancel.is_cancelled() {
        ErrorCode::Cancelled
    } else if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        ErrorCode::Timeout
    } else {
        ErrorCode::IoError
    }
}
pub(crate) fn transport_error(
    error: anyhow::Error,
    grant: &RequestGrant,
    cancel: &HttpCancel,
) -> ErrorCode {
    if let Some(code) = error.downcast_ref::<ErrorCode>() {
        return *code;
    }
    if let Some(http_err) = error.downcast_ref::<HttpClientError>() {
        return match http_err {
            HttpClientError::Cancelled => ErrorCode::Cancelled,
            HttpClientError::Timeout => ErrorCode::Timeout,
            HttpClientError::Denied => ErrorCode::Denied,
        };
    }
    if let Err(error) = grant.check_live() {
        return error;
    }
    if cancel.is_cancelled() {
        return ErrorCode::Cancelled;
    }
    if let Some(io) = error.downcast_ref::<io::Error>()
        && matches!(
            io.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        )
    {
        return ErrorCode::Timeout;
    }
    ErrorCode::IoError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_origin_referer_is_strict_origin_and_never_downgrades() {
        let https_cdn = Url::parse("https://upos-hz-mirrorakam.akamaized.net/video.m4s").unwrap();
        let http_cdn = Url::parse("http://cdn.example/video.m4s").unwrap();
        assert_eq!(
            cross_origin_referer(
                "https://www.bilibili.com/video/BV1?token=secret",
                &https_cdn
            )
            .as_deref(),
            Some("https://www.bilibili.com/")
        );
        assert_eq!(
            cross_origin_referer("https://site.example:8443/private/path", &https_cdn).as_deref(),
            Some("https://site.example:8443/")
        );
        assert_eq!(
            cross_origin_referer("https://www.bilibili.com/", &http_cdn),
            None
        );
        assert_eq!(
            cross_origin_referer("https://user:pass@www.bilibili.com/", &https_cdn),
            None
        );
        assert_eq!(
            cross_origin_referer("http://site.example/path?a=1", &http_cdn).as_deref(),
            Some("http://site.example/")
        );
    }
}
