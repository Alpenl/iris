//! Minimal synchronous HTTP/1.1 + HTTPS (`rustls`) client for bounded requests,
//! remote photo header probing, stream URL validation, and redirect SSRF protection.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
mod connection;

pub(crate) use connection::NetworkScope;
pub use connection::{HttpCancel, validate_stream_url};
use connection::{SocketRegistration, connect_transport, remaining_timeout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct HttpBytesResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

pub(crate) fn sanitize_header_name(name: &str) -> String {
    name.split(['\r', '\n', '\0'])
        .next()
        .unwrap_or("")
        .bytes()
        .filter(|&b| is_tchar(b))
        .map(char::from)
        .collect()
}

pub(crate) fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|&c| c != '\r' && c != '\n' && c != '\0' && (c == '\t' || (c as u32) >= 0x20))
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn sanitize_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            let name = sanitize_header_name(k);
            (!name.is_empty()).then(|| (name, sanitize_header_value(v)))
        })
        .collect()
}

pub(crate) fn origin_headers(
    url: &Url,
    headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use percent_encoding::percent_decode_str;
    let mut clean = sanitize_headers(headers);
    if (!url.username().is_empty() || url.password().is_some())
        && header_ci(&clean, "authorization").is_none()
    {
        let mut credentials: Vec<u8> = percent_decode_str(url.username()).collect();
        credentials.push(b':');
        credentials.extend(percent_decode_str(url.password().unwrap_or_default()));
        clean.insert(
            "Authorization".into(),
            format!("Basic {}", STANDARD.encode(credentials)),
        );
    }
    clean
}

fn header_ci<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

type ResponseReader = io::Chain<Cursor<Vec<u8>>, connection::Transport>;

struct ChunkedReader {
    inner: BufReader<ResponseReader>,
    remaining: u64,
    need_crlf: bool,
    eof: bool,
}

impl ChunkedReader {
    fn read_line_capped(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::with_capacity(64);
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated chunk framing",
                ));
            }
            let take = buf
                .iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if line.len() + take > MAX_CHUNK_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunk line too long",
                ));
            }
            line.extend_from_slice(&buf[..take]);
            self.inner.consume(take);
            if line.ends_with(b"\n") {
                break;
            }
        }
        if !line.ends_with(b"\r\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing chunk line CRLF",
            ));
        }
        line.truncate(line.len() - 2);
        Ok(line)
    }
}

impl Read for ChunkedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.eof {
            return Ok(0);
        }
        while self.remaining == 0 {
            if self.need_crlf {
                let mut end = [0; 2];
                self.inner.read_exact(&mut end)?;
                if end != *b"\r\n" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing chunk CRLF",
                    ));
                }
            }
            self.need_crlf = false;
            let line = self.read_line_capped()?;
            let hex = line.split(|&b| b == b';').next().unwrap_or_default();
            let hex = std::str::from_utf8(hex)
                .ok()
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            let size = u64::from_str_radix(hex, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                for _ in 0..64 {
                    if self.read_line_capped()?.is_empty() {
                        self.eof = true;
                        return Ok(0);
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "too many chunk trailers",
                ));
            }
            self.remaining = size;
        }
        let cap = (out.len() as u64).min(self.remaining) as usize;
        let n = self.inner.read(&mut out[..cap])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated chunk",
            ));
        }
        self.remaining -= n as u64;
        self.need_crlf = self.remaining == 0;
        Ok(n)
    }
}

enum HttpBody {
    Empty,
    Chunked(Box<ChunkedReader>),
    Fixed(ResponseReader, u64),
    CloseDelimited(ResponseReader),
}

impl Read for HttpBody {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Empty => Ok(0),
            Self::Chunked(body) => body.read(out),
            Self::CloseDelimited(body) => body.read(out),
            Self::Fixed(body, left) => {
                let cap = (*left).min(out.len() as u64) as usize;
                if cap == 0 {
                    return Ok(0);
                }
                let n = body.read(&mut out[..cap])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated HTTP body",
                    ));
                }
                *left -= n as u64;
                Ok(n)
            }
        }
    }
}

pub(crate) struct HttpStream {
    pub status: u16,
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    body: HttpBody,
    _socket: SocketRegistration,
}

impl HttpStream {
    pub(crate) fn body(&mut self) -> &mut impl Read {
        &mut self.body
    }
}

pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub url: Url,
    pub headers: &'a BTreeMap<String, String>,
    pub body: Option<&'a [u8]>,
    /// Generated by Iris or validated as a single numeric byte range at the relay.
    pub range: Option<&'a str>,
}

pub(crate) fn open_stream(
    req: Request<'_>,
    scope: &NetworkScope,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<HttpStream> {
    open_stream_with_credentials(req, scope.source(), scope, cancel, deadline)
}

fn open_stream_with_credentials(
    req: Request<'_>,
    credential_origin: &Url,
    scope: &NetworkScope,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<HttpStream> {
    let Request {
        method,
        url,
        headers,
        body,
        range,
    } = req;
    let mut cur_method = method.trim().to_ascii_uppercase();
    if cur_method.is_empty() || !cur_method.bytes().all(is_tchar) {
        bail!("invalid HTTP method");
    }
    let mut cur_headers = if url.origin() == credential_origin.origin() {
        origin_headers(credential_origin, headers)
    } else {
        BTreeMap::new()
    };
    if range.is_some() {
        cur_headers.retain(|k, _| !k.eq_ignore_ascii_case("range"));
    }
    let (mut cur_url, mut cur_body) = (url, body);
    let header_deadline = deadline.unwrap_or_else(|| Instant::now() + CONNECT_TIMEOUT + IO_TIMEOUT);

    for redirect in 0..=MAX_REDIRECTS {
        let (mut transport, registration) =
            connect_transport(&cur_url, scope, cancel, Some(header_deadline))?;
        let target = &cur_url[url::Position::BeforePath..url::Position::AfterQuery];
        let host_hdr = match (cur_url.scheme(), cur_url.port()) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => {
                cur_url.host_str().unwrap_or("localhost").to_string()
            }
            (_, Some(p)) => format!("{}:{p}", cur_url.host_str().unwrap_or("localhost")),
        };
        let mut wire = format!("{cur_method} {target} HTTP/1.1\r\nHost: {host_hdr}\r\n");
        if header_ci(&cur_headers, "User-Agent").is_none() {
            wire.push_str(concat!(
                "User-Agent: Iris/",
                env!("CARGO_PKG_VERSION"),
                "\r\n"
            ));
        }
        if let Some(range) = range {
            wire.push_str(&format!("Range: {range}\r\n"));
        }
        wire.push_str("Accept-Encoding: identity\r\n");
        for (k, v) in &cur_headers {
            if !matches!(
                k.to_ascii_lowercase().as_str(),
                "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
            ) {
                wire.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        if let Some(payload) = cur_body {
            wire.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        }
        wire.push_str("Connection: close\r\n\r\n");
        let mut wire_bytes = wire.into_bytes();
        if let Some(payload) = cur_body.filter(|b| !b.is_empty()) {
            if payload.len() <= 16 * 1024 {
                wire_bytes.extend_from_slice(payload);
                transport.write_all(&wire_bytes)?;
            } else {
                transport.write_all(&wire_bytes)?;
                transport.write_all(payload)?;
            }
        } else {
            transport.write_all(&wire_bytes)?;
        }
        transport.flush()?;

        let (mut raw, mut tmp) = (Vec::with_capacity(2048), [0u8; 4096]);
        let mut header_start = 0;
        let mut scan = 0;
        let mut interim = 0;
        let (status, header_end, mut resp_headers) = loop {
            remaining_timeout(cancel, Some(header_deadline), IO_TIMEOUT)?;
            let Some(i) = raw[scan..].windows(4).position(|w| w == b"\r\n\r\n") else {
                if raw.len() >= MAX_RESPONSE_HEADER_BYTES {
                    bail!("HTTP response headers exceed limit");
                }
                scan = raw.len().saturating_sub(3).max(header_start);
                let n = transport
                    .read(&mut tmp[..(MAX_RESPONSE_HEADER_BYTES - raw.len()).min(4096)])?;
                if n == 0 {
                    bail!("unexpected EOF reading HTTP response headers");
                }
                raw.extend_from_slice(&tmp[..n]);
                continue;
            };
            let end = scan + i + 4;
            let mut slots = [httparse::EMPTY_HEADER; 128];
            let mut parsed = httparse::Response::new(&mut slots);
            if !parsed.parse(&raw[header_start..end])?.is_complete() {
                bail!("incomplete HTTP response headers");
            }
            let status = parsed.code.context("missing HTTP status")?;
            if (100..200).contains(&status) {
                interim += 1;
                if status == 101 || interim > 8 {
                    bail!("unsupported HTTP informational response");
                }
                header_start = end;
                scan = end;
                continue;
            }
            let mut values = BTreeMap::<String, String>::new();
            for h in parsed.headers {
                let val = sanitize_header_value(&String::from_utf8_lossy(h.value));
                let name = h.name.to_ascii_lowercase();
                if let Some(prev) = values.get_mut(&name) {
                    match name.as_str() {
                        "content-length" if *prev == val => (),
                        "content-length" | "content-range" | "content-type" | "location" => {
                            bail!("duplicate HTTP {name} header");
                        }
                        _ => {
                            prev.push_str(", ");
                            prev.push_str(&val);
                        }
                    }
                } else {
                    values.insert(name, val);
                }
            }
            break (status, end, values);
        };

        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = header_ci(&resp_headers, "location").filter(|s| !s.is_empty())
        {
            if redirect == MAX_REDIRECTS {
                bail!("exceeded maximum HTTP redirects ({MAX_REDIRECTS})");
            }
            let next_url = validate_stream_url(cur_url.join(loc)?.as_str())?;
            if cur_url.scheme() == "https" && next_url.scheme() != "https" {
                bail!("refusing an HTTPS redirect to an insecure transport");
            }
            let changed_origin = cur_url.origin() != next_url.origin();
            if changed_origin {
                cur_headers.clear();
            }
            if (status == 303 && !matches!(cur_method.as_str(), "GET" | "HEAD"))
                || (matches!(status, 301 | 302) && cur_method == "POST")
            {
                cur_method = "GET".into();
                cur_body = None;
                cur_headers.retain(|k, _| !k.eq_ignore_ascii_case("content-type"));
            }
            if changed_origin && cur_body.is_some_and(|b| !b.is_empty()) {
                bail!("refusing to forward a request body across HTTP origins");
            }
            cur_url = next_url;
            continue;
        }

        transport.set_body_deadline(deadline)?;
        let content_len = resp_headers
            .get("content-length")
            .map(|s| s.parse::<u64>())
            .transpose()
            .context("invalid HTTP Content-Length")?;
        let mut prefix = Cursor::new(raw);
        prefix.set_position(header_end as u64);
        let chained = prefix.chain(transport);
        let is_chunked = match header_ci(&resp_headers, "transfer-encoding") {
            Some(te) if te.eq_ignore_ascii_case("chunked") => {
                resp_headers.remove("content-length");
                true
            }
            Some(_) => bail!("unsupported HTTP Transfer-Encoding"),
            None => false,
        };
        let body = if cur_method == "HEAD" || matches!(status, 204 | 304) {
            HttpBody::Empty
        } else if is_chunked {
            HttpBody::Chunked(Box::new(ChunkedReader {
                inner: BufReader::new(chained),
                remaining: 0,
                need_crlf: false,
                eof: false,
            }))
        } else if let Some(len) = content_len {
            HttpBody::Fixed(chained, len)
        } else {
            HttpBody::CloseDelimited(chained)
        };
        return Ok(HttpStream {
            status,
            url: cur_url,
            headers: resp_headers,
            body,
            _socket: registration,
        });
    }
    bail!("HTTP redirect loop")
}

fn read_bounded(
    stream: &mut HttpStream,
    max: usize,
    truncate: bool,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let declared = stream
        .headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    if !truncate && declared.is_some_and(|n| n > max as u64) {
        bail!("HTTP body exceeds byte limit");
    }
    let limit = if truncate { max } else { max.saturating_add(1) };
    let mut out = Vec::new();
    let mut buf = [0; 8192];
    while out.len() < limit {
        if Instant::now() >= deadline {
            bail!("HTTP request deadline exceeded");
        }
        let cap = (limit - out.len()).min(buf.len());
        let n = stream.body.read(&mut buf[..cap])?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    if out.len() > max {
        bail!("HTTP body exceeds byte limit");
    }
    if declared.is_some_and(|n| (out.len() as u64) < n.min(limit as u64)) {
        bail!("truncated HTTP body");
    }
    Ok(out)
}

pub(crate) fn request_bytes_cancellable(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&[u8]>,
    max_bytes: usize,
    cancel: &HttpCancel,
) -> Result<HttpBytesResponse> {
    let url = validate_stream_url(url)?;
    let scope = NetworkScope::new(url.clone());
    let deadline = Instant::now() + CONNECT_TIMEOUT + IO_TIMEOUT;
    let mut response = open_stream(
        Request {
            method,
            url,
            headers,
            body,
            range: None,
        },
        &scope,
        cancel,
        Some(deadline),
    )?;
    let body = if method.eq_ignore_ascii_case("HEAD") || matches!(response.status, 204 | 304) {
        Vec::new()
    } else {
        read_bounded(&mut response, max_bytes, false, deadline)?
    };
    Ok(HttpBytesResponse {
        status: response.status,
        body,
    })
}

pub(crate) fn fetch_subtitle_bytes(
    url: Url,
    headers: &BTreeMap<String, String>,
    credential_origin: &Url,
    scope: &NetworkScope,
    cancel: &HttpCancel,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + CONNECT_TIMEOUT + IO_TIMEOUT;
    let mut response = open_stream_with_credentials(
        Request {
            method: "GET",
            url,
            headers,
            body: None,
            range: None,
        },
        credential_origin,
        scope,
        cancel,
        Some(deadline),
    )?;
    if response.status != 200 {
        bail!("subtitle HTTP fetch failed with status {}", response.status);
    }
    if response
        .headers
        .get("content-encoding")
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        bail!("unsupported subtitle content encoding");
    }
    read_bounded(&mut response, 8 * 1024 * 1024, false, deadline)
}

pub fn fetch_head_bytes(
    url: &str,
    headers: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if max_bytes == 0 {
        validate_stream_url(url)?;
        return Ok(Vec::new());
    }
    let range = format!("bytes=0-{}", max_bytes - 1);
    let url = validate_stream_url(url)?;
    let scope = NetworkScope::new(url.clone());
    let deadline = Instant::now() + CONNECT_TIMEOUT + IO_TIMEOUT;
    let mut resp = open_stream(
        Request {
            method: "GET",
            url,
            headers,
            body: None,
            range: Some(&range),
        },
        &scope,
        &HttpCancel::default(),
        Some(deadline),
    )?;
    match resp.status {
        200 | 206 => read_bounded(&mut resp, max_bytes, true, deadline),
        416 => Ok(Vec::new()),
        s => bail!("upstream HTTP status {s} while probing header bytes"),
    }
}

pub(crate) fn fetch_range(
    url: &str,
    headers: &BTreeMap<String, String>,
    start: u64,
    len: usize,
    cancel: &HttpCancel,
) -> Result<(Vec<u8>, Option<u64>)> {
    let end = start
        .checked_add(len.checked_sub(1).context("empty byte range")? as u64)
        .context("byte range overflow")?;
    let range = format!("bytes={start}-{end}");
    let url = validate_stream_url(url)?;
    let scope = NetworkScope::new(url.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut response = open_stream(
        Request {
            method: "GET",
            url,
            headers,
            body: None,
            range: Some(&range),
        },
        &scope,
        cancel,
        Some(deadline),
    )?;
    if response
        .headers
        .get("content-encoding")
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        bail!("encoded byte range");
    }
    if response.status == 200 && start == 0 {
        return Ok((read_bounded(&mut response, len, true, deadline)?, None));
    }
    if response.status != 206 {
        bail!("range request returned HTTP {}", response.status);
    }
    let content_range = response
        .headers
        .get("content-range")
        .context("missing Content-Range")?;
    let (bounds, total) = content_range
        .strip_prefix("bytes ")
        .and_then(|v| v.split_once('/'))
        .context("invalid Content-Range")?;
    let (first, last) = bounds.split_once('-').context("invalid Content-Range")?;
    let (first, last, total) = (
        first.parse::<u64>()?,
        last.parse::<u64>()?,
        total.parse::<u64>()?,
    );
    if first != start || last < first || last > end || last >= total {
        bail!("unexpected Content-Range");
    }
    let size = (last - first + 1) as usize;
    let data = read_bounded(&mut response, size, false, deadline)?;
    if data.len() != size {
        bail!("truncated byte range");
    }
    Ok((data, Some(total)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn read_range(reply: &'static [u8], start: u64, len: usize) -> Result<(Vec<u8>, Option<u64>)> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let url = format!("http://{}/photo", listener.local_addr()?);
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut line = String::new();
            let mut reader = BufReader::new(&socket);
            while reader.read_line(&mut line).unwrap() > 0 && !line.ends_with("\r\n\r\n") {}
            assert!(line.contains(&format!(
                "Range: bytes={start}-{}\r\n",
                start + len as u64 - 1
            )));
            socket.write_all(reply).unwrap();
        });
        let result = fetch_range(&url, &BTreeMap::new(), start, len, &HttpCancel::default());
        server.join().unwrap();
        result
    }

    #[test]
    fn sparse_ranges_require_matching_offsets_size_and_identity_encoding() {
        assert_eq!(
            read_range(
                b"HTTP/1.1 206 OK\r\nContent-Range: bytes 4-5/6\r\nContent-Length: 2\r\n\r\nab",
                4,
                8
            )
            .unwrap(),
            (b"ab".to_vec(), Some(6))
        );
        assert_eq!(
            read_range(
                b"HTTP/1.1 206 OK\r\nContent-Range: bytes 4-5/10\r\nContent-Length: 2\r\n\r\nab",
                4,
                6
            )
            .unwrap(),
            (b"ab".to_vec(), Some(10))
        );
        assert_eq!(
            read_range(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd", 0, 2).unwrap(),
            (b"ab".to_vec(), None)
        );
        for invalid in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nab"[..],
            b"HTTP/1.1 206 OK\r\nContent-Range: bytes 3-4/6\r\n\r\nab",
            b"HTTP/1.1 206 OK\r\nContent-Range: bytes 4-5/6\r\n\r\na",
            b"HTTP/1.1 206 OK\r\nContent-Range: bytes 4-5/6\r\n\r\nabc",
            b"HTTP/1.1 206 OK\r\nContent-Range: bytes 4-5/6\r\nContent-Encoding: gzip\r\n\r\nab",
        ] {
            assert!(read_range(invalid, 4, 2).is_err());
        }
    }
}
