//! Shared loopback HTTP/1.1 nonblocking I/O primitives used by both direct
//! HTTP playback relays and plugin-scoped stream relays.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mio::{Interest, Registry, Token};

use crate::net::http_client::HttpCancel;

struct ClientState {
    cancel: HttpCancel,
    writable: Arc<(Mutex<u64>, Condvar)>,
}

/// Multiplexes client disconnect cancellation and nonblocking write readiness
/// onto a relay listener's `mio::Poll` loop.
pub struct ClientHub {
    registry: Registry,
    next_token: AtomicUsize,
    clients: Mutex<BTreeMap<usize, ClientState>>,
}

impl ClientHub {
    pub fn new(registry: Registry, first_token: usize) -> Self {
        Self {
            registry,
            next_token: AtomicUsize::new(first_token),
            clients: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn watch(
        self: &Arc<Self>,
        socket: &TcpStream,
        cancel: HttpCancel,
    ) -> io::Result<ClientWatch> {
        let token = self
            .next_token
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("relay client token exhausted"))?;
        let cloned = socket.try_clone()?;
        let mut stream = mio::net::TcpStream::from_std(cloned);
        let writable = Arc::new((Mutex::new(0u64), Condvar::new()));
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token,
                ClientState {
                    cancel,
                    writable: writable.clone(),
                },
            );
        if let Err(err) = self
            .registry
            .register(&mut stream, Token(token), Interest::READABLE)
        {
            self.clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&token);
            return Err(err);
        }
        Ok(ClientWatch {
            hub: Arc::clone(self),
            token,
            stream,
            writable,
            armed_writable: false,
        })
    }

    /// Processes a client `mio` event (any token >= `first_token`).
    pub fn dispatch_event(&self, event: &mio::event::Event) {
        let Token(id) = event.token();
        if event.is_readable()
            || event.is_read_closed()
            || event.is_write_closed()
            || event.is_error()
        {
            let client = self
                .clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(client) = client {
                client.cancel.cancel();
                let mut epoch = client.writable.0.lock().unwrap_or_else(|e| e.into_inner());
                *epoch = epoch.wrapping_add(1);
                client.writable.1.notify_all();
            }
        } else if event.is_writable() {
            let writable = self
                .clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .map(|c| c.writable.clone());
            if let Some(writable) = writable {
                let mut epoch = writable.0.lock().unwrap_or_else(|e| e.into_inner());
                *epoch = epoch.wrapping_add(1);
                writable.1.notify_all();
            }
        }
    }
}

pub struct ClientWatch {
    hub: Arc<ClientHub>,
    token: usize,
    stream: mio::net::TcpStream,
    writable: Arc<(Mutex<u64>, Condvar)>,
    armed_writable: bool,
}

impl ClientWatch {
    pub fn writable_gen(&self) -> u64 {
        *self.writable.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn wait_writable(&mut self, seen_gen: u64, cancel: &HttpCancel) -> io::Result<()> {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        if let Err(err) = self.hub.registry.reregister(
            &mut self.stream,
            Token(self.token),
            Interest::READABLE | Interest::WRITABLE,
        ) {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "relay cancelled",
                ));
            }
            return Err(err);
        }
        self.armed_writable = true;
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        let guard = self.writable.0.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == seen_gen {
            let _ = self
                .writable
                .1
                .wait_timeout(guard, Duration::from_millis(100));
        }
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        Ok(())
    }

    pub fn disarm_writable(&mut self) {
        if self.armed_writable {
            self.armed_writable = false;
            let _ = self.hub.registry.reregister(
                &mut self.stream,
                Token(self.token),
                Interest::READABLE,
            );
        }
    }
}

impl Drop for ClientWatch {
    fn drop(&mut self) {
        self.hub
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
        let _ = self.hub.registry.deregister(&mut self.stream);
    }
}

/// Parsed and security-validated HTTP/1.1 loopback relay request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedHttpRequest<'a> {
    pub method: HttpMethod,
    pub path: &'a str,
    pub range: Option<&'a str>,
}

/// Strict loopback HTTP/1.1 request validation failure with canonical HTTP status mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpRequestError {
    BadRequest,
    Forbidden,
    MethodNotAllowed,
    PayloadTooLarge,
    RangeNotSatisfiable,
}

impl HttpRequestError {
    pub const fn status_code(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Forbidden => 403,
            Self::MethodNotAllowed => 405,
            Self::PayloadTooLarge => 413,
            Self::RangeNotSatisfiable => 416,
        }
    }

    pub const fn reason_phrase(self) -> &'static str {
        match self {
            Self::BadRequest => "Bad Request",
            Self::Forbidden => "Forbidden",
            Self::MethodNotAllowed => "Method Not Allowed",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RangeNotSatisfiable => "Range Not Satisfiable",
        }
    }
}

/// Strictly parses and validates an HTTP/1.1 `GET`/`HEAD` request targeted at a loopback relay.
///
/// Enforces:
/// - HTTP/1.1 complete header block with no trailing bytes (`400`)
/// - Single exact `Host == expected_host` and no browser `Origin`/`Sec-Fetch-*` headers (`403`)
/// - `GET` or `HEAD` method only (`405`)
/// - No `Transfer-Encoding` and no non-zero/duplicate/invalid `Content-Length` (`400`/`413`)
///   so unread request body bytes cannot trigger `ClientHub::dispatch_event` self-cancellation
/// - At most one valid UTF-8 `Range` header (`416`)
pub fn parse_http_request<'a>(
    raw: &'a [u8],
    expected_host: &str,
) -> Result<ParsedHttpRequest<'a>, HttpRequestError> {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut slots);
    let used = match request.parse(raw) {
        Ok(httparse::Status::Complete(n)) => n,
        _ => return Err(HttpRequestError::BadRequest),
    };
    if request.version != Some(1) {
        return Err(HttpRequestError::BadRequest);
    }
    let method_str = request.method.ok_or(HttpRequestError::BadRequest)?;
    let path = request
        .path
        .filter(|p| !p.is_empty())
        .ok_or(HttpRequestError::BadRequest)?;

    let mut host_seen = false;
    let mut transfer_encoding = false;
    let mut content_length_seen = false;
    let mut content_length_err = None;
    let mut range: Option<&'a str> = None;
    let mut range_err = false;

    for h in request.headers {
        if h.name.eq_ignore_ascii_case("origin")
            || (h.name.len() >= 10 && h.name.as_bytes()[..10].eq_ignore_ascii_case(b"sec-fetch-"))
        {
            return Err(HttpRequestError::Forbidden);
        }
        if h.name.eq_ignore_ascii_case("host") {
            if host_seen || h.value != expected_host.as_bytes() {
                return Err(HttpRequestError::Forbidden);
            }
            host_seen = true;
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = true;
        } else if h.name.eq_ignore_ascii_case("content-length") {
            let value = h.value.trim_ascii();
            if content_length_seen || value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                content_length_err = Some(HttpRequestError::BadRequest);
            } else if value.iter().any(|&b| b != b'0') {
                content_length_err.get_or_insert(HttpRequestError::PayloadTooLarge);
            } else if value != b"0" {
                content_length_err = Some(HttpRequestError::BadRequest);
            }
            content_length_seen = true;
        } else if h.name.eq_ignore_ascii_case("range") {
            if range.is_some() {
                range_err = true;
            } else if let Ok(value) = std::str::from_utf8(h.value) {
                range = Some(value);
            } else {
                range_err = true;
            }
        }
    }

    if !host_seen {
        return Err(HttpRequestError::Forbidden);
    }
    let method = match method_str {
        "GET" => HttpMethod::Get,
        "HEAD" => HttpMethod::Head,
        _ => return Err(HttpRequestError::MethodNotAllowed),
    };
    if transfer_encoding {
        return Err(HttpRequestError::BadRequest);
    }
    if let Some(err) = content_length_err {
        return Err(err);
    }
    if used != raw.len() {
        return Err(HttpRequestError::BadRequest);
    }
    if range_err {
        return Err(HttpRequestError::RangeNotSatisfiable);
    }
    Ok(ParsedHttpRequest {
        method,
        path,
        range,
    })
}

/// Reads an HTTP/1.1 request header block terminated by `\r\n\r\n` within `timeout` and `max_bytes`.
pub fn read_request(
    socket: &mut TcpStream,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    socket.set_nonblocking(false)?;
    let deadline = Instant::now() + timeout;
    let mut raw = Vec::with_capacity(2048);
    let mut buf = [0u8; 2048];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "relay request timeout"))?;
        socket.set_read_timeout(Some(left))?;
        let n = match socket.read(&mut buf) {
            Ok(n) => n,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "relay request timeout",
                ));
            }
            Err(err) => return Err(err),
        };
        if n == 0 || raw.len() + n > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid relay request size",
            ));
        }
        let scan = raw.len().saturating_sub(3);
        raw.extend_from_slice(&buf[..n]);
        if raw[scan..].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(raw);
        }
    }
}

/// Clears blocking read/write socket timeouts (`SO_SNDTIMEO` / `SO_RCVTIMEO`)
/// and switches a loopback client socket into nonblocking mode before body writes.
///
/// On Windows Winsock, leaving `SO_SNDTIMEO` armed during streaming writes can
/// leave partial bytes queued after `WSAETIMEDOUT` and corrupt subsequent chunks.
pub fn enter_nonblocking_body(socket: &mut TcpStream) -> io::Result<()> {
    socket.set_write_timeout(None)?;
    socket.set_read_timeout(None)?;
    socket.set_nonblocking(true)?;
    debug_assert_eq!(socket.write_timeout().ok().flatten(), None);
    debug_assert_eq!(socket.read_timeout().ok().flatten(), None);
    Ok(())
}

const MAX_NONBLOCKING_WRITE: usize = 64 * 1024;

/// Writes `bytes` to a nonblocking loopback socket, waiting for `mio` writability
/// and aborting promptly if `cancel` fires.
pub fn write_all_cancellable(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if cancel.is_cancelled() {
            watch.disarm_writable();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        let epoch = watch.writable_gen();
        let step = &bytes[..bytes.len().min(MAX_NONBLOCKING_WRITE)];
        match socket.write(step) {
            Ok(0) => {
                watch.disarm_writable();
                return Err(io::Error::new(io::ErrorKind::WriteZero, "relay write zero"));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Err(wait_err) = watch.wait_writable(epoch, cancel) {
                    watch.disarm_writable();
                    return Err(wait_err);
                }
            }
            Err(err) => {
                watch.disarm_writable();
                return Err(err);
            }
        }
    }
    watch.disarm_writable();
    Ok(())
}

/// Writes pre-body bytes (`headers` / status responses) to a nonblocking loopback
/// socket within `timeout`, failing closed with `TimedOut` if the client stalls.
pub fn write_all_bounded(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !bytes.is_empty() {
        if cancel.is_cancelled() {
            watch.disarm_writable();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        if Instant::now() >= deadline {
            watch.disarm_writable();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "relay pre-body write timeout",
            ));
        }
        let epoch = watch.writable_gen();
        let step = &bytes[..bytes.len().min(MAX_NONBLOCKING_WRITE)];
        match socket.write(step) {
            Ok(0) => {
                watch.disarm_writable();
                return Err(io::Error::new(io::ErrorKind::WriteZero, "relay write zero"));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    watch.disarm_writable();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "relay pre-body write timeout",
                    ));
                }
                if let Err(wait_err) = watch.wait_writable(epoch, cancel) {
                    watch.disarm_writable();
                    return Err(wait_err);
                }
            }
            Err(err) => {
                watch.disarm_writable();
                return Err(err);
            }
        }
    }
    watch.disarm_writable();
    Ok(())
}

/// Writes one payload slice, optionally wrapped in HTTP/1.1 chunked framing.
pub fn write_chunk_cancellable(
    socket: &mut TcpStream,
    bytes: &[u8],
    chunked: bool,
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if chunked {
        let header = format!("{:x}\r\n", bytes.len());
        write_all_cancellable(socket, header.as_bytes(), watch, cancel)?;
    }
    write_all_cancellable(socket, bytes, watch, cancel)?;
    if chunked {
        write_all_cancellable(socket, b"\r\n", watch, cancel)?;
    }
    Ok(())
}

/// Returns `true` when `bytes` begins like an HLS (`#EXTM3U`) or XML/DASH (`<...`) manifest.
pub fn is_adaptive_manifest(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let trimmed = bytes.trim_ascii_start();
    trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"#EXTM3U"))
        || trimmed == b"<"
        || (trimmed.len() >= 2
            && trimmed[0] == b'<'
            && matches!(trimmed[1], b'?' | b'!' | b'a'..=b'z' | b'A'..=b'Z' | b'/'))
}

/// Returns `true` while `bytes` is still too short to decide whether it begins with
/// an adaptive manifest header.
pub fn is_adaptive_prefix_undecided(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes == b"\xef" || bytes == b"\xef\xbb" {
        return true;
    }
    let trimmed = bytes
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(bytes)
        .trim_ascii_start();
    trimmed.is_empty()
        || (trimmed.len() < 7 && b"#EXTM3U"[..trimmed.len()].eq_ignore_ascii_case(trimmed))
        || (trimmed.len() < 2 && trimmed[0] == b'<')
}

#[cfg(test)]
mod tests {
    use super::{
        HttpMethod, HttpRequestError, ParsedHttpRequest, is_adaptive_manifest,
        is_adaptive_prefix_undecided, parse_http_request,
    };

    const HOST: &str = "127.0.0.1:43210";

    #[test]
    fn parse_http_request_accepts_valid_get_head_and_zero_content_length() {
        let req = b"GET /stream/item.m3u8 HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nRange: bytes=0-1023\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            parse_http_request(req, HOST),
            Ok(ParsedHttpRequest {
                method: HttpMethod::Get,
                path: "/stream/item.m3u8",
                range: Some("bytes=0-1023"),
            })
        );
        let head = b"HEAD /seg.ts HTTP/1.1\r\nHost: 127.0.0.1:43210\r\n\r\n";
        assert_eq!(
            parse_http_request(head, HOST),
            Ok(ParsedHttpRequest {
                method: HttpMethod::Head,
                path: "/seg.ts",
                range: None,
            })
        );
    }

    #[test]
    fn parse_http_request_distinguishes_status_codes_and_rejects_bodies() {
        assert_eq!(
            parse_http_request(b"GET /a HTTP/1.0\r\nHost: 127.0.0.1:43210\r\n\r\n", HOST),
            Err(HttpRequestError::BadRequest)
        );
        assert_eq!(
            parse_http_request(b"GET /a HTTP/1.1\r\n\r\n", HOST),
            Err(HttpRequestError::Forbidden)
        );
        assert_eq!(
            parse_http_request(b"GET /a HTTP/1.1\r\nHost: evil.example\r\n\r\n", HOST),
            Err(HttpRequestError::Forbidden)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nOrigin: https://evil.example\r\n\r\n",
                HOST
            ),
            Err(HttpRequestError::Forbidden)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nSec-Fetch-Site: cross-site\r\n\r\n",
                HOST
            ),
            Err(HttpRequestError::Forbidden)
        );
        assert_eq!(
            parse_http_request(
                b"POST /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nContent-Length: 5\r\n\r\nhello",
                HOST
            ),
            Err(HttpRequestError::MethodNotAllowed)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nTransfer-Encoding: chunked\r\n\r\n",
                HOST
            ),
            Err(HttpRequestError::BadRequest)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nContent-Length: 5\r\n\r\nhello",
                HOST
            ),
            Err(HttpRequestError::PayloadTooLarge)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
                HOST
            ),
            Err(HttpRequestError::BadRequest)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\n\r\nextra",
                HOST
            ),
            Err(HttpRequestError::BadRequest)
        );
        assert_eq!(
            parse_http_request(
                b"GET /a HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nRange: bytes=0-1\r\nRange: bytes=2-3\r\n\r\n",
                HOST
            ),
            Err(HttpRequestError::RangeNotSatisfiable)
        );
        assert_eq!(HttpRequestError::BadRequest.status_code(), 400);
        assert_eq!(HttpRequestError::Forbidden.status_code(), 403);
        assert_eq!(HttpRequestError::MethodNotAllowed.status_code(), 405);
        assert_eq!(HttpRequestError::PayloadTooLarge.status_code(), 413);
        assert_eq!(HttpRequestError::RangeNotSatisfiable.status_code(), 416);
    }

    #[test]
    fn adaptive_prefix_helpers_track_leading_whitespace_bom_and_partial_headers() {
        let mut padded_hls = vec![b' '; 128];
        padded_hls.extend_from_slice(b"#EXTM3U\n");
        assert!(is_adaptive_prefix_undecided(&padded_hls[..64]));
        assert!(!is_adaptive_manifest(&padded_hls[..64]));
        assert!(is_adaptive_prefix_undecided(&padded_hls[..131])); // 128 spaces + "#EX"
        assert!(is_adaptive_manifest(&padded_hls));
        assert!(!is_adaptive_prefix_undecided(&padded_hls));

        let bom_xml = b"\xef\xbb\xbf  \n\t<?xml version=\"1.0\"?><MPD/>";
        assert!(is_adaptive_prefix_undecided(&bom_xml[..2]));
        assert!(is_adaptive_prefix_undecided(&bom_xml[..6]));
        assert!(is_adaptive_manifest(bom_xml));

        let mp4 = b"\x00\x00\x00\x18ftypisom";
        assert!(!is_adaptive_prefix_undecided(&mp4[..1]));
        assert!(!is_adaptive_manifest(mp4));
    }

    #[test]
    fn enter_nonblocking_body_clears_socket_timeouts_and_write_all_bounded_times_out_stalled_peer()
    {
        use super::{ClientHub, enter_nonblocking_body, write_all_bounded};
        use crate::net::http_client::HttpCancel;
        use std::io::{ErrorKind, Read};
        use std::net::{Ipv4Addr, TcpListener, TcpStream};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();

        server
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(server.write_timeout().unwrap().is_some());
        assert!(server.read_timeout().unwrap().is_some());

        enter_nonblocking_body(&mut server).unwrap();
        assert_eq!(server.write_timeout().unwrap(), None);
        assert_eq!(server.read_timeout().unwrap(), None);
        let mut probe = [0u8; 1];
        assert_eq!(
            server.read(&mut probe).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        let poll = mio::Poll::new().unwrap();
        let hub = Arc::new(ClientHub::new(poll.registry().try_clone().unwrap(), 2));
        let cancel = HttpCancel::default();
        let mut watch = hub.watch(&server, cancel.clone()).unwrap();

        // Repeatedly write 1 MiB chunks without reading on `client` until both the
        // peer receive window and local send buffer saturate (Windows Winsock loopback
        // auto-tuning can buffer >= 8-16 MiB before returning `WouldBlock`).
        let chunk = vec![0x5au8; 1 << 20];
        let start = Instant::now();
        let mut timed_out = None;
        for _ in 0..256 {
            if let Err(err) = write_all_bounded(
                &mut server,
                &chunk,
                &mut watch,
                &cancel,
                Duration::from_millis(120),
            ) {
                timed_out = Some(err);
                break;
            }
        }
        let err = timed_out.expect("stalled loopback peer never saturated kernel buffers");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(3));
        let _ = client.read(&mut probe);
    }
}
