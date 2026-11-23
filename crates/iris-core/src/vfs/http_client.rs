//! Minimal synchronous HTTP/1.1 + HTTPS (`rustls`) client for bounded requests,
//! remote photo header probing, stream URL validation, and redirect SSRF protection.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rustls_pki_types::ServerName;
use url::{Host, Url};

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

fn classify_ip(ip: IpAddr) -> (bool, bool, bool) {
    match ip {
        IpAddr::V4(v4) => (
            v4.is_loopback(),
            v4.octets()[0] == 169 && v4.octets()[1] == 254,
            v4.is_unspecified(),
        ),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(m) => classify_ip(IpAddr::V4(m)),
            None => (
                v6.is_loopback(),
                (v6.segments()[0] & 0xffc0) == 0xfe80,
                v6.is_unspecified(),
            ),
        },
    }
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(v4)) => v4.is_loopback(),
        Some(Host::Ipv6(v6)) => classify_ip(IpAddr::V6(v6)).0,
        Some(Host::Domain(d)) => {
            d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
        }
        None => false,
    }
}

/// Validates a remote media or bridge URL (`http`/`https` only; loopback requires port `>= 1024`).
pub fn validate_stream_url(raw: &str) -> Result<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.bytes().any(|b| b < 0x20 || b == 0x7F) {
        bail!("URL is empty or contains forbidden control characters");
    }
    let parsed = Url::parse(trimmed).with_context(|| format!("invalid URL: {trimmed}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "unsupported URL scheme '{}' (expected http or https)",
            parsed.scheme()
        );
    }
    match parsed.host() {
        Some(Host::Ipv4(v4)) if classify_ip(IpAddr::V4(v4)).1 || v4.is_unspecified() => {
            bail!("forbidden IPv4 stream host: {v4}");
        }
        Some(Host::Ipv6(v6)) if classify_ip(IpAddr::V6(v6)).1 || classify_ip(IpAddr::V6(v6)).2 => {
            bail!("forbidden IPv6 stream host: {v6}");
        }
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => {}
        Some(Host::Domain(d)) if !d.is_empty() => {}
        _ => bail!("URL is missing host"),
    }
    if is_loopback_url(&parsed) && parsed.port_or_known_default().unwrap_or(80) < 1024 {
        bail!("loopback stream URL port must be >= 1024");
    }
    Ok(parsed)
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

fn header_ci<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<std::result::Result<Arc<rustls::ClientConfig>, String>> =
        OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS config failed: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(cfg))
        })
        .clone()
        .map_err(|e| anyhow!("{e}"))
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

fn connect_transport(url: &Url, forbid_loopback: bool) -> Result<Transport> {
    let raw_host = url.host_str().ok_or_else(|| anyhow!("missing URL host"))?;
    let host = raw_host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_host);
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    if addrs.is_empty() {
        bail!("no socket addresses resolved for {host}:{port}");
    }
    for addr in &addrs {
        let (loopback, link_local, unspec) = classify_ip(addr.ip());
        if link_local || unspec || (forbid_loopback && loopback) {
            bail!("blocked disallowed target IP {} for {host}", addr.ip());
        }
    }
    let tcp = addrs
        .into_iter()
        .find_map(|addr| TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok())
        .ok_or_else(|| anyhow!("connection failed to {host}:{port}"))?;
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
    if url.scheme() == "https" {
        let sni = ServerName::try_from(host.to_string()).map_err(|_| anyhow!("bad SNI: {host}"))?;
        let conn = rustls::ClientConnection::new(tls_config()?, sni)?;
        Ok(Transport::Tls(Box::new(rustls::StreamOwned::new(
            conn, tcp,
        ))))
    } else {
        Ok(Transport::Plain(tcp))
    }
}

struct ChunkedReader<R: Read> {
    inner: BufReader<R>,
    remaining: u64,
    need_crlf: bool,
    eof: bool,
}

impl<R: Read> ChunkedReader<R> {
    fn read_line_capped(&mut self) -> io::Result<String> {
        let mut line = Vec::with_capacity(64);
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                break;
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
        String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.eof {
            return Ok(0);
        }
        while self.remaining == 0 {
            if self.need_crlf
                && !self
                    .read_line_capped()?
                    .trim_matches(['\r', '\n'])
                    .is_empty()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing chunk CRLF",
                ));
            }
            self.need_crlf = false;
            let line = self.read_line_capped()?;
            let hex = line
                .trim_end_matches(['\r', '\n'])
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            let size = u64::from_str_radix(hex, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                for _ in 0..64 {
                    if self
                        .read_line_capped()?
                        .trim_end_matches(['\r', '\n'])
                        .is_empty()
                    {
                        break;
                    }
                }
                self.eof = true;
                return Ok(0);
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

fn execute_bounded(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&[u8]>,
    max_bytes: usize,
    truncate_stream: bool,
) -> Result<HttpBytesResponse> {
    let mut cur_method = method.trim().to_ascii_uppercase();
    if cur_method.is_empty() || !cur_method.bytes().all(is_tchar) {
        bail!("invalid HTTP method");
    }
    let initial_url = validate_stream_url(url)?;
    let initial_loopback = is_loopback_url(&initial_url);
    let (mut cur_url, mut cur_headers, mut cur_body) =
        (initial_url, sanitize_headers(headers), body);

    for redirect in 0..=MAX_REDIRECTS {
        let mut transport = connect_transport(&cur_url, redirect > 0 && !initial_loopback)?;
        let target = match &cur_url[url::Position::BeforePath..] {
            "" => "/",
            s => s,
        };
        let host_hdr = match (cur_url.scheme(), cur_url.port()) {
            ("http", Some(80)) | ("https", Some(443)) | (_, None) => {
                cur_url.host_str().unwrap_or("localhost").to_string()
            }
            (_, Some(p)) => format!("{}:{p}", cur_url.host_str().unwrap_or("localhost")),
        };
        let mut wire = format!("{cur_method} {target} HTTP/1.1\r\nHost: {host_hdr}\r\n");
        if header_ci(&cur_headers, "User-Agent").is_none() {
            wire.push_str("User-Agent: Iris/0.1\r\n");
        }
        for (k, v) in &cur_headers {
            if !matches!(
                k.to_ascii_lowercase().as_str(),
                "host" | "connection" | "content-length" | "transfer-encoding"
            ) {
                wire.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        if let Some(payload) = cur_body {
            wire.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        }
        wire.push_str("Connection: close\r\n\r\n");
        transport.write_all(wire.as_bytes())?;
        if let Some(payload) = cur_body.filter(|b| !b.is_empty()) {
            transport.write_all(payload)?;
        }
        transport.flush()?;

        let (mut raw, mut tmp) = (Vec::with_capacity(2048), [0u8; 4096]);
        let header_end = loop {
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if raw.len() >= MAX_RESPONSE_HEADER_BYTES {
                bail!("HTTP response headers exceed limit");
            }
            let n =
                transport.read(&mut tmp[..(MAX_RESPONSE_HEADER_BYTES - raw.len()).min(4096)])?;
            if n == 0 {
                bail!("unexpected EOF reading HTTP response headers");
            }
            raw.extend_from_slice(&tmp[..n]);
        };

        let mut slots = [httparse::EMPTY_HEADER; 128];
        let mut parsed = httparse::Response::new(&mut slots);
        if matches!(parsed.parse(&raw[..header_end])?, httparse::Status::Partial) {
            bail!("incomplete HTTP response headers");
        }
        let status = parsed.code.ok_or_else(|| anyhow!("missing HTTP status"))?;
        let mut resp_headers = BTreeMap::<String, String>::new();
        for h in parsed.headers.iter().filter(|h| !h.name.is_empty()) {
            let val = sanitize_header_value(&String::from_utf8_lossy(h.value));
            resp_headers
                .entry(h.name.to_ascii_lowercase())
                .and_modify(|e| {
                    e.push_str(", ");
                    e.push_str(&val);
                })
                .or_insert(val);
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = header_ci(&resp_headers, "location").filter(|s| !s.is_empty())
        {
            if redirect == MAX_REDIRECTS {
                bail!("exceeded maximum HTTP redirects ({MAX_REDIRECTS})");
            }
            let next_url = validate_stream_url(cur_url.join(loc)?.as_str())?;
            if !initial_loopback && is_loopback_url(&next_url) {
                bail!("redirect from external host to loopback is forbidden");
            }
            let scheme_ok = cur_url.scheme() == next_url.scheme()
                || (cur_url.scheme() == "http" && next_url.scheme() == "https");
            let same_origin = scheme_ok
                && cur_url
                    .host_str()
                    .zip(next_url.host_str())
                    .is_some_and(|(x, y)| x.eq_ignore_ascii_case(y))
                && cur_url.port_or_known_default() == next_url.port_or_known_default();
            if !same_origin {
                cur_headers.retain(|k, _| {
                    !matches!(
                        k.to_ascii_lowercase().as_str(),
                        "authorization" | "proxy-authorization" | "cookie"
                    )
                });
            }
            if (status == 303 && !matches!(cur_method.as_str(), "GET" | "HEAD"))
                || (matches!(status, 301 | 302) && cur_method == "POST")
            {
                cur_method = "GET".into();
                cur_body = None;
                cur_headers.retain(|k, _| !k.eq_ignore_ascii_case("content-type"));
            }
            cur_url = next_url;
            continue;
        }

        let content_len = header_ci(&resp_headers, "content-length")
            .and_then(|s| s.split(',').next()?.trim().parse::<u64>().ok());
        if !truncate_stream && content_len.is_some_and(|len| len > max_bytes as u64) {
            bail!("HTTP Content-Length exceeds limit ({max_bytes})");
        }
        let chained = Cursor::new(raw[header_end..].to_vec()).chain(transport);
        let is_chunked = header_ci(&resp_headers, "transfer-encoding").is_some_and(|te| {
            te.split(',')
                .any(|p| p.trim().eq_ignore_ascii_case("chunked"))
        });
        let read_cap = if truncate_stream {
            max_bytes as u64
        } else {
            (max_bytes as u64).saturating_add(1)
        };
        let mut out = Vec::new();
        if cur_method != "HEAD" && !(100..200).contains(&status) && status != 204 && status != 304 {
            if is_chunked {
                let mut r = ChunkedReader {
                    inner: BufReader::with_capacity(8 * 1024, chained),
                    remaining: 0,
                    need_crlf: false,
                    eof: false,
                };
                (&mut r).take(read_cap).read_to_end(&mut out)?;
            } else {
                chained
                    .take(content_len.map_or(read_cap, |cl| cl.min(read_cap)))
                    .read_to_end(&mut out)?;
            }
        }
        if !truncate_stream && out.len() > max_bytes {
            bail!("HTTP response body exceeds {max_bytes} bytes limit");
        }
        return Ok(HttpBytesResponse { status, body: out });
    }
    bail!("HTTP redirect loop")
}

pub(crate) fn request_bytes(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&[u8]>,
    max_bytes: usize,
) -> Result<HttpBytesResponse> {
    execute_bounded(method, url, headers, body, max_bytes, false)
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
    let mut req_headers = sanitize_headers(headers);
    req_headers.retain(|k, _| !k.eq_ignore_ascii_case("Range"));
    req_headers.insert("Range".into(), format!("bytes=0-{}", max_bytes - 1));
    let resp = execute_bounded("GET", url, &req_headers, None, max_bytes, true)?;
    if resp.status == 416 {
        return Ok(Vec::new());
    }
    if resp.status != 200 && resp.status != 206 {
        bail!(
            "upstream HTTP status {} while probing header bytes",
            resp.status
        );
    }
    Ok(resp.body)
}
