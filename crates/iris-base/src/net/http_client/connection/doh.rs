//! Encrypted DNS-over-HTTPS (RFC 8484) fallback when a local TUN proxy hijacks
//! port-53 DNS with IANA benchmark Fake-IPs (`198.18.0.0/15`, `2001:2::/48`).
//!
//! Privacy boundary: the fallback sends the hostname to the public resolvers in
//! [`active_resolvers`] and then connects to the real address, which can bypass a
//! proxy's domain rules. It is **off by default** (`fake_ip_doh_fallback = false`)
//! so private media hostnames (e.g. self-hosted WebDAV/NAS) are never silently
//! disclosed to third-party resolvers; users can explicitly opt in.
//!
//! Security boundary: Fake-IP ranges remain `AddressKind::Forbidden` permanently.
//! We never relax `address_kind` or `RequestGrant::check_address` to connect
//! directly to `198.18.0.0/15` or `2001:2::/48`; instead, when opted in, we
//! resolve authoritative `A` records over verified TLS to numeric `AddressKind::Public`
//! DoH resolvers and subject those real IPs to the exact same SSRF/grant checks.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls_pki_types::{IpAddr as PkiIpAddr, ServerName};
use url::{Host, Url};

use super::super::address::{AddressKind, address_kind};
use super::super::{ChunkedReader, parse_decimal_u64};
use super::{HttpCancel, TimedSocket, connect_tcp, remaining_timeout, tls_config};

const DOH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_DOH_HEADER_BYTES: usize = 16 * 1024;
const MAX_DOH_BODY_BYTES: usize = 4096;
const MAX_CACHE_ENTRIES: usize = 128;
const MAX_DOH_RESOLVERS: usize = 8;
const MIN_TTL_SECS: u64 = 15;
const MAX_TTL_SECS: u64 = 300;

/// Default numeric public DoH endpoints with IP SANs in their TLS certificates.
pub const DEFAULT_DOH_RESOLVERS: [&str; 3] = [
    "223.5.5.5",  // AliDNS
    "1.12.12.12", // DNSPod
    "1.1.1.1",    // Cloudflare
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DohEndpoint {
    pub ip: IpAddr,
    pub port: u16,
    pub path: String,
}

impl DohEndpoint {
    fn host_header(&self) -> String {
        match (self.ip, self.port) {
            (IpAddr::V4(v4), 443) => v4.to_string(),
            (IpAddr::V4(v4), port) => format!("{v4}:{port}"),
            (IpAddr::V6(v6), 443) => format!("[{v6}]"),
            (IpAddr::V6(v6), port) => format!("[{v6}]:{port}"),
        }
    }

    fn canonical_string(&self) -> String {
        if self.port == 443 && self.path == "/dns-query" {
            return self.ip.to_string();
        }
        let host = match self.ip {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        if self.port == 443 {
            format!("https://{host}{}", self.path)
        } else {
            format!("https://{host}:{}{}", self.port, self.path)
        }
    }
}

fn default_endpoints() -> Vec<DohEndpoint> {
    DEFAULT_DOH_RESOLVERS
        .iter()
        .map(|raw| parse_doh_endpoint(raw).expect("valid default DoH endpoint"))
        .collect()
}

fn valid_doh_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 128
        && path.starts_with('/')
        && !path.starts_with("//")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
}

pub(crate) fn parse_doh_endpoint(raw: &str) -> Option<DohEndpoint> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 256 || trimmed.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let (ip, port, path) = if let Ok(ip) = trimmed.parse::<IpAddr>() {
        (ip.to_canonical(), 443, "/dns-query".to_owned())
    } else if let Ok(sock) = trimmed.parse::<SocketAddr>() {
        if sock.port() == 0 {
            return None;
        }
        (
            sock.ip().to_canonical(),
            sock.port(),
            "/dns-query".to_owned(),
        )
    } else if trimmed.starts_with("https://") {
        let parsed = Url::parse(trimmed).ok()?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return None;
        }
        let ip = match parsed.host()? {
            Host::Ipv4(v4) => IpAddr::V4(v4),
            Host::Ipv6(v6) => IpAddr::V6(v6).to_canonical(),
            Host::Domain(_) => return None,
        };
        let port = parsed.port_or_known_default()?;
        if port == 0 {
            return None;
        }
        let path = if parsed.path() == "/" {
            "/dns-query".to_owned()
        } else {
            parsed.path().to_owned()
        };
        if !valid_doh_path(&path) {
            return None;
        }
        (ip, port, path)
    } else {
        return None;
    };
    if address_kind(ip) != AddressKind::Public {
        return None;
    }
    Some(DohEndpoint { ip, port, path })
}

/// Validates and normalizes one public numeric DoH resolver entry for `settings.conf`.
pub fn normalize_doh_resolver(raw: &str) -> Option<String> {
    parse_doh_endpoint(raw).map(|ep| ep.canonical_string())
}

struct CachedDns {
    ips: Vec<IpAddr>,
    expires: Instant,
}

fn cache_insert(
    cache: &mut BTreeMap<String, CachedDns>,
    key: String,
    ips: Vec<IpAddr>,
    expires: Instant,
    now: Instant,
    max_entries: usize,
) {
    cache.retain(|_, v| v.expires > now);
    if !cache.contains_key(&key)
        && cache.len() >= max_entries
        && let Some(earliest) = cache
            .iter()
            .min_by_key(|(_, v)| v.expires)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&earliest);
    }
    cache.insert(key, CachedDns { ips, expires });
}

#[derive(Debug, PartialEq, Eq)]
enum DohFraming {
    Chunked,
    Fixed(usize),
    CloseDelimited,
}

fn parse_doh_framing(headers: &[httparse::Header<'_>]) -> Result<DohFraming> {
    let mut content_len: Option<usize> = None;
    let mut chunked = false;
    for h in headers {
        if h.name.eq_ignore_ascii_case("content-length") {
            if content_len.is_some() {
                bail!("duplicate DoH Content-Length header");
            }
            let s = std::str::from_utf8(h.value)?.trim();
            let parsed = parse_decimal_u64(s)
                .and_then(|n| usize::try_from(n).ok())
                .context("bad DoH Content-Length")?;
            if parsed > MAX_DOH_BODY_BYTES {
                bail!("DoH body exceeds limit");
            }
            content_len = Some(parsed);
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked {
                bail!("duplicate DoH Transfer-Encoding header");
            }
            let s = std::str::from_utf8(h.value)?.trim();
            if !s.eq_ignore_ascii_case("chunked") {
                bail!("unsupported DoH Transfer-Encoding");
            }
            chunked = true;
        } else if h.name.eq_ignore_ascii_case("content-encoding") {
            let s = std::str::from_utf8(h.value)?.trim();
            if !s.eq_ignore_ascii_case("identity") {
                bail!("unsupported DoH Content-Encoding");
            }
        }
    }
    if chunked && content_len.is_some() {
        bail!("conflicting DoH Content-Length and Transfer-Encoding");
    }
    Ok(if chunked {
        DohFraming::Chunked
    } else if let Some(len) = content_len {
        DohFraming::Fixed(len)
    } else {
        DohFraming::CloseDelimited
    })
}

struct DohInner {
    endpoints: Option<Vec<DohEndpoint>>,
    fallback_enabled: bool,
    fallback_notice: bool,
    epoch: u64,
    cache: BTreeMap<String, CachedDns>,
}

impl DohInner {
    fn active_endpoints(&self) -> Vec<DohEndpoint> {
        self.endpoints.clone().unwrap_or_else(default_endpoints)
    }

    fn bump_epoch_and_reset(&mut self) {
        self.cache.clear();
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("DoH config epoch overflow");
        self.fallback_notice = false;
    }
}

/// Module-private instantiable DoH configuration, epoch, and response cache state.
/// Production callers share [`DEFAULT_DOH`]; unit tests create isolated instances.
struct DohState {
    inner: Mutex<DohInner>,
}

impl DohState {
    const fn new() -> Self {
        Self {
            inner: Mutex::new(DohInner {
                endpoints: None,
                fallback_enabled: false,
                fallback_notice: false,
                epoch: 0,
                cache: BTreeMap::new(),
            }),
        }
    }

    fn epoch(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .epoch
    }

    fn set_resolvers(&self, resolvers: &[String]) -> Result<()> {
        if resolvers.is_empty() || resolvers.len() > MAX_DOH_RESOLVERS {
            bail!("DoH resolver list must contain 1..={MAX_DOH_RESOLVERS} entries");
        }
        let mut parsed = Vec::with_capacity(resolvers.len());
        for raw in resolvers {
            let ep = parse_doh_endpoint(raw)
                .ok_or_else(|| anyhow!("invalid or non-public DoH resolver: {raw}"))?;
            if !parsed.contains(&ep) {
                parsed.push(ep);
            }
        }
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = match &guard.endpoints {
            Some(prev) => prev != &parsed,
            None => default_endpoints() != parsed,
        };
        if changed {
            guard.endpoints = Some(parsed);
            guard.bump_epoch_and_reset();
        }
        Ok(())
    }

    fn active_resolvers(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active_endpoints()
            .into_iter()
            .map(|ep| ep.canonical_string())
            .collect()
    }

    fn set_fallback_enabled(&self, enabled: bool) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.fallback_enabled != enabled {
            guard.fallback_enabled = enabled;
            guard.bump_epoch_and_reset();
        }
    }

    fn fallback_enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fallback_enabled
    }

    fn use_fallback(&self, host: &str, resolved: &[IpAddr]) -> bool {
        if !should_bypass_fake_ip(host, resolved) {
            return false;
        }
        let (enabled, should_log, servers) = {
            let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let enabled = guard.fallback_enabled;
            let should_log = !std::mem::replace(&mut guard.fallback_notice, true);
            let servers = if should_log && enabled {
                guard
                    .active_endpoints()
                    .into_iter()
                    .map(|ep| ep.canonical_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                String::new()
            };
            (enabled, should_log, servers)
        };
        if should_log {
            if enabled {
                log::info!(
                    "system DNS returned benchmark Fake-IP addresses (local TUN proxy?); \
                     resolving through configured public DoH resolvers ({servers}). \
                     Set fake_ip_doh_fallback = false to keep hostnames on the system resolver"
                );
            } else {
                log::warn!(
                    "system DNS returned benchmark Fake-IP addresses and the public DoH \
                     fallback is disabled (default); requests to such hosts are refused"
                );
            }
        }
        enabled
    }

    fn resolve_a_records(
        &self,
        host: &str,
        cancel: &HttpCancel,
        current_deadline: impl Fn() -> Instant,
    ) -> Result<Vec<IpAddr>> {
        self.resolve_a_records_with(host, cancel, current_deadline, query_single_server)
    }

    fn resolve_a_records_with(
        &self,
        host: &str,
        cancel: &HttpCancel,
        current_deadline: impl Fn() -> Instant,
        mut query_fn: impl FnMut(
            &DohEndpoint,
            &str,
            &HttpCancel,
            &dyn Fn() -> Instant,
        ) -> Result<(Vec<IpAddr>, u64)>,
    ) -> Result<Vec<IpAddr>> {
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        let (start_epoch, servers) = {
            let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if !guard.fallback_enabled {
                bail!("public DoH fallback is disabled");
            }
            let now = Instant::now();
            if let Some(hit) = guard
                .cache
                .get(&key)
                .filter(|entry| entry.expires > now)
                .map(|entry| entry.ips.clone())
            {
                return Ok(hit);
            }
            (guard.epoch, guard.active_endpoints())
        };

        let wire_query = encode_dns_a_query(&key)?;
        let dns_param = URL_SAFE_NO_PAD.encode(&wire_query);
        let (ips, ttl_secs) = query_servers(
            &servers,
            || self.epoch() == start_epoch,
            cancel,
            current_deadline,
            |endpoint, deadline_fn| query_fn(endpoint, &dns_param, cancel, deadline_fn),
        )?;

        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.epoch != start_epoch {
            if !guard.fallback_enabled {
                bail!("public DoH fallback disabled during lookup");
            }
            bail!("DoH configuration changed during lookup");
        }
        let now = Instant::now();
        let expires = now + Duration::from_secs(ttl_secs.clamp(MIN_TTL_SECS, MAX_TTL_SECS));
        cache_insert(
            &mut guard.cache,
            key,
            ips.clone(),
            expires,
            now,
            MAX_CACHE_ENTRIES,
        );
        Ok(ips)
    }
}

static DEFAULT_DOH: DohState = DohState::new();

/// Applies a validated list of public numeric DoH resolvers (`1..=8` entries).
pub fn set_resolvers(resolvers: &[String]) -> Result<()> {
    DEFAULT_DOH.set_resolvers(resolvers)
}

pub fn active_resolvers() -> Vec<String> {
    DEFAULT_DOH.active_resolvers()
}

/// Applies the user's `fake_ip_doh_fallback` setting. Turning it off also drops
/// cached DoH answers so no earlier public resolution is reused.
pub fn set_fallback_enabled(enabled: bool) {
    DEFAULT_DOH.set_fallback_enabled(enabled);
}

pub fn fallback_enabled() -> bool {
    DEFAULT_DOH.fallback_enabled()
}

/// Decides whether a system DNS answer goes through the public DoH fallback.
/// Logs once per setting change; the hostname is never logged.
pub fn use_fallback(host: &str, resolved: &[IpAddr]) -> bool {
    DEFAULT_DOH.use_fallback(host, resolved)
}

pub(crate) fn is_benchmark_fake_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 198 && (b == 18 || b == 19)
        }
        IpAddr::V6(v6) => {
            let [a, b, c, ..] = v6.segments();
            a == 0x2001 && b == 0x0002 && c == 0
        }
    }
}

pub(crate) fn should_bypass_fake_ip(host: &str, resolved: &[IpAddr]) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty()
        || h.len() > 253
        || !h.contains('.')
        || h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.parse::<IpAddr>().is_ok()
    {
        return false;
    }
    !resolved.is_empty() && resolved.iter().copied().any(is_benchmark_fake_ip)
}

pub fn resolve_a_records(
    host: &str,
    cancel: &HttpCancel,
    current_deadline: impl Fn() -> Instant,
) -> Result<Vec<IpAddr>> {
    DEFAULT_DOH.resolve_a_records(host, cancel, current_deadline)
}

fn query_servers<T>(
    servers: &[DohEndpoint],
    epoch_unchanged: impl Fn() -> bool,
    cancel: &HttpCancel,
    current_deadline: impl Fn() -> Instant,
    mut query: impl FnMut(&DohEndpoint, &dyn Fn() -> Instant) -> Result<T>,
) -> Result<T> {
    let mut last_err = anyhow!("all DoH resolvers failed");
    loop {
        let round_deadline = current_deadline();
        for endpoint in servers {
            if !epoch_unchanged() {
                return Err(anyhow!("DoH configuration changed during lookup"));
            }
            if cancel.is_cancelled() || Instant::now() >= current_deadline() {
                break;
            }
            match query(endpoint, &current_deadline) {
                Ok(val) => return Ok(val),
                Err(err) => last_err = err,
            }
        }
        if !epoch_unchanged() {
            return Err(anyhow!("DoH configuration changed during lookup"));
        }
        if cancel.is_cancelled() {
            break;
        }
        let latest = current_deadline();
        if latest <= round_deadline || Instant::now() >= latest {
            break;
        }
    }
    Err(last_err)
}

/// Parses a complete buffered HTTP/1.1 DoH response (`200 OK`, framing, and RFC 1035 DNS body).
pub fn parse_doh_response_bytes(raw: &[u8]) -> Result<(Vec<IpAddr>, u64)> {
    let Some(i) = raw
        .get(..raw.len().min(MAX_DOH_HEADER_BYTES))
        .and_then(|s| s.windows(4).position(|w| w == b"\r\n\r\n"))
    else {
        bail!("missing or oversized DoH HTTP header terminator");
    };
    let header_end = i + 4;
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut slots);
    if !parsed.parse(&raw[..header_end])?.is_complete() {
        bail!("incomplete DoH HTTP headers");
    }
    if parsed.code != Some(200) {
        bail!("DoH HTTP status {:?}", parsed.code);
    }
    let framing = parse_doh_framing(parsed.headers)?;
    read_doh_body_and_parse(framing, Cursor::new(&raw[header_end..]))
}

fn read_doh_body_and_parse<R: Read>(framing: DohFraming, reader: R) -> Result<(Vec<IpAddr>, u64)> {
    let mut body = Vec::with_capacity(512);
    match framing {
        DohFraming::Chunked => {
            ChunkedReader::new(reader)
                .take(MAX_DOH_BODY_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
        }
        DohFraming::Fixed(len) => {
            reader.take(len as u64).read_to_end(&mut body)?;
            if body.len() != len {
                bail!("truncated DoH body");
            }
        }
        DohFraming::CloseDelimited => {
            reader
                .take(MAX_DOH_BODY_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
        }
    }
    if body.len() > MAX_DOH_BODY_BYTES {
        bail!("DoH body exceeds limit");
    }
    parse_dns_a_response(&body)
}

fn query_single_server(
    endpoint: &DohEndpoint,
    dns_param: &str,
    cancel: &HttpCancel,
    current_deadline: &dyn Fn() -> Instant,
) -> Result<(Vec<IpAddr>, u64)> {
    let attempt_cap = Instant::now() + DOH_ATTEMPT_TIMEOUT;
    let step_end = || current_deadline().min(attempt_cap);
    let step_timeout = remaining_timeout(cancel, Some(step_end()), DOH_ATTEMPT_TIMEOUT)?;
    let tcp = connect_tcp(
        SocketAddr::new(endpoint.ip, endpoint.port),
        cancel,
        Some(step_end()),
    )?;
    let _reg = cancel.register(&tcp)?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(step_timeout))?;
    tcp.set_write_timeout(Some(step_timeout))?;

    let mut timed = TimedSocket {
        tcp,
        deadline: Some(step_end()),
        idle_timeout: DOH_ATTEMPT_TIMEOUT,
    };
    let sni = ServerName::IpAddress(PkiIpAddr::from(endpoint.ip));
    let mut tls_conn = rustls::ClientConnection::new(tls_config()?, sni)?;
    while tls_conn.is_handshaking() {
        let end = step_end();
        timed.deadline = Some(end);
        remaining_timeout(cancel, Some(end), DOH_ATTEMPT_TIMEOUT)?;
        tls_conn.complete_io(&mut timed)?;
    }
    timed.deadline = Some(step_end());
    let mut stream = rustls::StreamOwned::new(tls_conn, timed);

    let host_hdr = endpoint.host_header();
    let req = format!(
        "GET {}?dns={dns_param} HTTP/1.1\r\nHost: {host_hdr}\r\nAccept: application/dns-message\r\nConnection: close\r\n\r\n",
        endpoint.path
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let end = step_end();
        stream.sock.deadline = Some(end);
        remaining_timeout(cancel, Some(end), DOH_ATTEMPT_TIMEOUT)?;
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if raw.len() >= MAX_DOH_HEADER_BYTES {
            bail!("DoH response headers too large");
        }
        let cap = (MAX_DOH_HEADER_BYTES - raw.len()).min(2048);
        let n = stream.read(&mut tmp[..cap])?;
        if n == 0 {
            bail!("EOF reading DoH response headers");
        }
        raw.extend_from_slice(&tmp[..n]);
    };

    let mut slots = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut slots);
    if !parsed.parse(&raw[..header_end])?.is_complete() {
        bail!("incomplete DoH HTTP headers");
    }
    if parsed.code != Some(200) {
        bail!("DoH HTTP status {:?}", parsed.code);
    }
    let framing = parse_doh_framing(parsed.headers)?;
    stream.sock.deadline = Some(step_end());

    let mut prefix = Cursor::new(raw);
    prefix.set_position(header_end as u64);
    let chained = prefix.chain(super::Transport::Tls(Box::new(stream)));
    read_doh_body_and_parse(framing, chained)
}

pub fn encode_dns_a_query(host: &str) -> Result<Vec<u8>> {
    let mut msg = Vec::with_capacity(64);
    // Header: ID=0 (RFC 8484 §4.1), Flags=0x0100 (RD=1), QDCOUNT=1, AN/NS/AR=0
    msg.extend_from_slice(&[0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("invalid DNS label in DoH query");
        }
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0);
    // QTYPE = A (1), QCLASS = IN (1)
    msg.extend_from_slice(&[0, 1, 0, 1]);
    Ok(msg)
}

fn skip_dns_name(msg: &[u8], mut pos: usize) -> Result<usize> {
    let mut jumps = 0usize;
    loop {
        let b = *msg.get(pos).context("truncated DNS name")?;
        if b == 0 {
            return Ok(pos + 1);
        }
        if (b & 0xC0) == 0xC0 {
            let b2 = *msg.get(pos + 1).context("truncated DNS name pointer")?;
            let target = (((b & 0x3F) as usize) << 8) | (b2 as usize);
            if target >= msg.len() {
                bail!("DNS name pointer out of bounds");
            }
            return Ok(pos + 2);
        }
        if (b & 0xC0) != 0 {
            bail!("unsupported DNS label compression bits");
        }
        pos = pos
            .checked_add(1 + b as usize)
            .filter(|&end| end <= msg.len())
            .context("DNS label exceeds packet")?;
        jumps += 1;
        if jumps > 128 {
            bail!("DNS name too many labels");
        }
    }
}

pub fn parse_dns_a_response(msg: &[u8]) -> Result<(Vec<IpAddr>, u64)> {
    if msg.len() < 12 {
        bail!("DNS response too short");
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    if (flags & 0x8000) == 0 || (flags & 0x000F) != 0 {
        bail!("DNS response error flags=0x{flags:04x}");
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let ancount = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    if qdcount > 8 || ancount > 64 {
        bail!("DNS response section count out of bounds");
    }
    let mut pos = 12usize;
    for _ in 0..qdcount {
        pos = skip_dns_name(msg, pos)?;
        pos = pos
            .checked_add(4)
            .filter(|&end| end <= msg.len())
            .context("truncated DNS question")?;
    }
    let mut ips = Vec::new();
    let mut min_ttl = MAX_TTL_SECS;
    for _ in 0..ancount {
        pos = skip_dns_name(msg, pos)?;
        let header = msg.get(pos..pos + 10).context("truncated DNS RR header")?;
        let rtype = u16::from_be_bytes([header[0], header[1]]);
        let rclass = u16::from_be_bytes([header[2], header[3]]);
        let ttl = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as u64;
        let rdlen = u16::from_be_bytes([header[8], header[9]]) as usize;
        pos += 10;
        let rdata = msg
            .get(pos..pos + rdlen)
            .context("truncated DNS RR rdata")?;
        pos += rdlen;
        if rtype == 1 && rclass == 1 && rdlen == 4 {
            let ip = IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
            if !is_benchmark_fake_ip(ip) && !ips.contains(&ip) {
                ips.push(ip);
                min_ttl = min_ttl.min(ttl);
            }
        }
    }
    if ips.is_empty() {
        bail!("DoH response contained no real A records");
    }
    Ok((ips, min_ttl))
}

#[cfg(test)]
mod tests {
    use super::{
        CachedDns, DEFAULT_DOH_RESOLVERS, DohFraming, DohState, HttpCancel, cache_insert,
        default_endpoints, encode_dns_a_query, normalize_doh_resolver, parse_dns_a_response,
        parse_doh_framing, parse_doh_response_bytes, query_servers, should_bypass_fake_ip,
    };
    use anyhow::anyhow;
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};

    #[test]
    fn detects_fake_ip_without_affecting_local_or_public_records() {
        assert!(should_bypass_fake_ip(
            "passport.bilibili.com",
            &[
                "198.18.1.82".parse().unwrap(),
                "2001:2::135".parse().unwrap(),
            ]
        ));
        assert!(!should_bypass_fake_ip(
            "nas.local",
            &["198.18.1.82".parse().unwrap()]
        ));
        assert!(!should_bypass_fake_ip(
            "localhost",
            &["198.18.1.82".parse().unwrap()]
        ));
        assert!(!should_bypass_fake_ip(
            "intranet.corp",
            &["192.168.1.50".parse().unwrap()]
        ));
        assert!(!should_bypass_fake_ip(
            "passport.bilibili.com",
            &["14.17.92.73".parse().unwrap()]
        ));
    }

    #[test]
    fn defaults_off_and_mid_flight_disable_aborts_without_caching() {
        let state = DohState::new();
        let fake: [IpAddr; 1] = ["198.18.1.82".parse().unwrap()];
        let real: [IpAddr; 1] = ["14.17.92.73".parse().unwrap()];
        assert!(!state.fallback_enabled(), "DoH fallback defaults to off");
        assert_eq!(state.epoch(), 0);
        assert!(!state.use_fallback("passport.bilibili.com", &fake));

        state.set_fallback_enabled(true);
        assert_eq!(state.epoch(), 1);
        assert!(state.use_fallback("passport.bilibili.com", &fake));
        assert!(!state.use_fallback("passport.bilibili.com", &real));
        state
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cache
            .insert(
                "cached.example".into(),
                CachedDns {
                    ips: real.to_vec(),
                    expires: Instant::now() + Duration::from_secs(60),
                },
            );

        // No-op write leaves cache and epoch untouched.
        state.set_fallback_enabled(true);
        assert_eq!(state.epoch(), 1);
        assert!(
            state
                .inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cache
                .contains_key("cached.example")
        );

        state.set_fallback_enabled(false);
        assert_eq!(state.epoch(), 2);
        assert!(!state.use_fallback("passport.bilibili.com", &fake));
        assert!(
            !state
                .inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cache
                .contains_key("cached.example"),
            "opting out must drop cached public answers"
        );

        // Mid-flight disable: turning off fallback while query_fn is in flight errors and skips caching.
        state.set_fallback_enabled(true);
        let err = state
            .resolve_a_records_with(
                "midflight.example",
                &HttpCancel::default(),
                || Instant::now() + Duration::from_secs(5),
                |_ep, _param, _cancel, _dl| {
                    state.set_fallback_enabled(false);
                    Ok((real.to_vec(), 60))
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");
        assert!(
            !state
                .inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cache
                .contains_key("midflight.example")
        );
    }

    #[test]
    fn configurable_resolvers_accept_public_numeric_endpoints_and_reject_non_public() {
        let state = DohState::new();
        assert_eq!(state.active_resolvers(), DEFAULT_DOH_RESOLVERS);

        assert_eq!(
            normalize_doh_resolver(" https://1.1.1.1/dns-query "),
            Some("1.1.1.1".into())
        );
        assert_eq!(
            normalize_doh_resolver("https://[2606:4700:4700::1111]:8443/custom"),
            Some("https://[2606:4700:4700::1111]:8443/custom".into())
        );
        for bad in [
            "",
            "dns.alidns.com",
            "https://dns.google/dns-query",
            "http://1.1.1.1/dns-query",
            "127.0.0.1",
            "192.168.1.1",
            "10.0.0.1",
            "198.18.0.2",
            "2001:2::1",
            "https://1.1.1.1/dns-query?extra=1",
        ] {
            assert_eq!(normalize_doh_resolver(bad), None, "must reject {bad}");
        }

        // Setting the default resolvers again is a no-op.
        state
            .set_resolvers(&DEFAULT_DOH_RESOLVERS.map(String::from))
            .unwrap();
        assert_eq!(state.epoch(), 0);

        state
            .set_resolvers(&[
                "8.8.8.8".into(),
                "https://9.9.9.9:8443/dns-query".into(),
                "8.8.8.8".into(),
            ])
            .unwrap();
        assert_eq!(state.epoch(), 1);
        assert_eq!(
            state.active_resolvers(),
            vec!["8.8.8.8", "https://9.9.9.9:8443/dns-query"]
        );
        state
            .set_resolvers(&["8.8.8.8".into(), "https://9.9.9.9:8443/dns-query".into()])
            .unwrap();
        assert_eq!(state.epoch(), 1);
        assert!(state.set_resolvers(&[]).is_err());
        assert!(state.set_resolvers(&["198.18.0.1".into()]).is_err());
    }

    #[test]
    fn encodes_and_parses_rfc1035_cname_plus_a_wire_messages() {
        let q = encode_dns_a_query("passport.bilibili.com").unwrap();
        assert_eq!(&q[..12], &[0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);

        let mut resp = Vec::new();
        resp.extend_from_slice(&[0, 0, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0]);
        resp.extend_from_slice(&q[12..]);
        resp.extend_from_slice(&[
            0xC0, 0x0C, 0, 5, 0, 1, 0, 0, 1, 44, 0, 5, 3, b'c', b'd', b'n', 0,
        ]);
        resp.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 45, 0, 4, 14, 17, 92, 73]);

        let (ips, ttl) = parse_dns_a_response(&resp).unwrap();
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(14, 17, 92, 73))]);
        assert_eq!(ttl, 45);

        let mut http_doh = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\n\r\n",
            resp.len()
        )
        .into_bytes();
        http_doh.extend_from_slice(&resp);
        assert_eq!(
            parse_doh_response_bytes(&http_doh).unwrap(),
            (vec![IpAddr::V4(Ipv4Addr::new(14, 17, 92, 73))], 45)
        );
    }

    #[test]
    fn doh_cache_evicts_earliest_expiry_and_headers_reject_duplicates() {
        let state = DohState::new();
        state.set_fallback_enabled(true);
        let now = Instant::now();
        let ip = vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))];
        let mut cache: BTreeMap<String, CachedDns> = BTreeMap::new();
        cache_insert(
            &mut cache,
            "a.example".into(),
            ip.clone(),
            now + Duration::from_secs(120),
            now,
            2,
        );
        cache_insert(
            &mut cache,
            "z.example".into(),
            ip.clone(),
            now + Duration::from_secs(30),
            now,
            2,
        );
        cache_insert(
            &mut cache,
            "m.example".into(),
            ip,
            now + Duration::from_secs(90),
            now,
            2,
        );
        assert!(cache.contains_key("a.example"));
        assert!(cache.contains_key("m.example"));
        assert!(!cache.contains_key("z.example"));

        let h = |name: &'static str, value: &'static [u8]| httparse::Header { name, value };
        assert_eq!(
            parse_doh_framing(&[h("Content-Length", b"42")]).unwrap(),
            DohFraming::Fixed(42)
        );
        assert_eq!(
            parse_doh_framing(&[h("Transfer-Encoding", b"chunked")]).unwrap(),
            DohFraming::Chunked
        );
        assert_eq!(parse_doh_framing(&[]).unwrap(), DohFraming::CloseDelimited);
        assert!(
            parse_doh_framing(&[h("Content-Length", b"42"), h("Content-Length", b"42")]).is_err()
        );
        assert!(
            parse_doh_framing(&[h("Content-Length", b"42"), h("Content-Length", b"10")]).is_err()
        );
        assert!(
            parse_doh_framing(&[
                h("Content-Length", b"42"),
                h("Transfer-Encoding", b"chunked"),
            ])
            .is_err()
        );
        assert!(parse_doh_framing(&[h("Transfer-Encoding", b"gzip")]).is_err());
        assert!(parse_doh_framing(&[h("Content-Length", b"+42")]).is_err());
        assert!(parse_doh_framing(&[h("Content-Encoding", b"gzip")]).is_err());

        let cancel = HttpCancel::default();
        cancel.cancel();
        assert!(
            state
                .resolve_a_records("uncached.example", &cancel, || now + Duration::from_secs(5))
                .is_err()
        );

        let shared_deadline = Mutex::new(Instant::now() + Duration::from_millis(50));
        let mut attempts = 0usize;
        let active_cancel = HttpCancel::default();
        let servers = default_endpoints();
        let rescued = query_servers(
            &servers,
            || true,
            &active_cancel,
            || *shared_deadline.lock().unwrap(),
            |_ep, deadline_fn| {
                attempts += 1;
                if attempts == 1 {
                    *shared_deadline.lock().unwrap() = Instant::now() + Duration::from_secs(5);
                    Err(anyhow!("initial short deadline expired"))
                } else {
                    assert!(deadline_fn() > Instant::now() + Duration::from_secs(1));
                    Ok(Ipv4Addr::new(14, 17, 92, 73))
                }
            },
        )
        .unwrap();
        assert_eq!(rescued, Ipv4Addr::new(14, 17, 92, 73));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn epoch_rejects_mid_flight_resolver_and_aba_toggle_changes_and_isolates_instances() {
        let cancel = HttpCancel::default();
        let dl = || Instant::now() + Duration::from_secs(5);
        let ip_a = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let ip_b = IpAddr::V4(Ipv4Addr::new(151, 101, 1, 69));

        // 1. Unchanged configuration caches and returns result; subsequent lookup hits cache.
        let state = DohState::new();
        state.set_fallback_enabled(true);
        state
            .set_resolvers(&["8.8.8.8".into(), "1.1.1.1".into()])
            .unwrap();
        let mut calls = 0usize;
        let got = state
            .resolve_a_records_with("stable.example", &cancel, dl, |ep, _p, _c, _d| {
                calls += 1;
                assert_eq!(ep.ip, "8.8.8.8".parse::<IpAddr>().unwrap());
                Ok((vec![ip_a], 120))
            })
            .unwrap();
        assert_eq!(got, vec![ip_a]);
        assert_eq!(calls, 1);
        let cached = state
            .resolve_a_records_with("stable.example", &cancel, dl, |_ep, _p, _c, _d| {
                unreachable!("must be served from cache")
            })
            .unwrap();
        assert_eq!(cached, vec![ip_a]);

        // 2. Resolver list change mid-query: aborts remaining old endpoints, returns Err, and skips cache.
        let mut tried_ips = Vec::new();
        let err = state
            .resolve_a_records_with("switch-fail.example", &cancel, dl, |ep, _p, _c, _d| {
                tried_ips.push(ep.ip);
                state.set_resolvers(&["9.9.9.9".into()]).unwrap();
                Err(anyhow!("first resolver failed"))
            })
            .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert_eq!(
            tried_ips,
            vec!["8.8.8.8".parse::<IpAddr>().unwrap()],
            "epoch change must stop querying remaining old servers immediately"
        );

        let err = state
            .resolve_a_records_with("switch-ok.example", &cancel, dl, |ep, _p, _c, _d| {
                assert_eq!(ep.ip, "9.9.9.9".parse::<IpAddr>().unwrap());
                state.set_resolvers(&["223.5.5.5".into()]).unwrap();
                Ok((vec![ip_a], 300))
            })
            .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(
            !state
                .inner
                .lock()
                .unwrap()
                .cache
                .contains_key("switch-ok.example"),
            "late answer from previous resolver list must neither return Ok nor enter cache"
        );

        // 3. Fallback toggled off -> on (ABA) mid-query: returns Err and skips cache.
        let err = state
            .resolve_a_records_with("toggle-aba.example", &cancel, dl, |_ep, _p, _c, _d| {
                state.set_fallback_enabled(false);
                state.set_fallback_enabled(true);
                Ok((vec![ip_a], 300))
            })
            .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(
            !state
                .inner
                .lock()
                .unwrap()
                .cache
                .contains_key("toggle-aba.example"),
            "pre-opt-out answer must neither return Ok nor enter cache after re-enable"
        );

        // 4. Two DohState instances keep configurations, epochs, and caches isolated.
        let other = DohState::new();
        assert!(!other.fallback_enabled());
        assert_eq!(other.active_resolvers(), DEFAULT_DOH_RESOLVERS);
        other.set_fallback_enabled(true);
        other.set_resolvers(&["1.0.0.1".into()]).unwrap();

        let got_state = state
            .resolve_a_records_with("shared.example", &cancel, dl, |ep, _p, _c, _d| {
                assert_eq!(ep.ip, "223.5.5.5".parse::<IpAddr>().unwrap());
                Ok((vec![ip_a], 120))
            })
            .unwrap();
        let got_other = other
            .resolve_a_records_with("shared.example", &cancel, dl, |ep, _p, _c, _d| {
                assert_eq!(ep.ip, "1.0.0.1".parse::<IpAddr>().unwrap());
                Ok((vec![ip_b], 120))
            })
            .unwrap();
        assert_eq!(got_state, vec![ip_a]);
        assert_eq!(got_other, vec![ip_b]);

        other.set_fallback_enabled(false);
        assert!(state.fallback_enabled());
        assert_eq!(
            state
                .resolve_a_records_with("shared.example", &cancel, dl, |_ep, _p, _c, _d| {
                    unreachable!("state cache must not be cleared by other instance")
                })
                .unwrap(),
            vec![ip_a]
        );
    }
}
