//! Encrypted DNS-over-HTTPS (RFC 8484) fallback when a local TUN proxy hijacks
//! port-53 DNS with IANA benchmark Fake-IPs (`198.18.0.0/15`, `2001:2::/48`).
//!
//! Security boundary: Fake-IP ranges remain `AddressKind::Forbidden`. We never
//! relax `address_kind` or `RequestGrant::check_address`; instead we resolve the
//! domain's real authoritative `A` records over verified TLS to numeric public
//! DoH resolvers and subject those real IPs to the exact same SSRF/grant checks.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls_pki_types::{IpAddr as PkiIpAddr, ServerName};

use super::super::{ChunkedReader, parse_decimal_u64};
use super::{HttpCancel, TimedSocket, connect_tcp, remaining_timeout, tls_config};

const DOH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_DOH_HEADER_BYTES: usize = 16 * 1024;
const MAX_DOH_BODY_BYTES: usize = 4096;
const MAX_CACHE_ENTRIES: usize = 128;
const MIN_TTL_SECS: u64 = 15;
const MAX_TTL_SECS: u64 = 300;

/// Public numeric DoH endpoints with IP SANs in their TLS certificates.
const DOH_SERVERS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(223, 5, 5, 5),  // AliDNS
    Ipv4Addr::new(1, 12, 12, 12), // DNSPod
    Ipv4Addr::new(1, 1, 1, 1),    // Cloudflare
];

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

static DOH_CACHE: Mutex<BTreeMap<String, CachedDns>> = Mutex::new(BTreeMap::new());

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

pub(crate) fn resolve_a_records(
    host: &str,
    cancel: &HttpCancel,
    current_deadline: impl Fn() -> Instant,
) -> Result<Vec<IpAddr>> {
    let key = host.trim_end_matches('.').to_ascii_lowercase();
    let now = Instant::now();
    if let Some(hit) = DOH_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .filter(|entry| entry.expires > now)
        .map(|entry| entry.ips.clone())
    {
        return Ok(hit);
    }

    let wire_query = encode_dns_a_query(&key)?;
    let dns_param = URL_SAFE_NO_PAD.encode(&wire_query);
    let (ips, ttl_secs) = query_servers(cancel, current_deadline, |server_ip, deadline_fn| {
        query_single_server(server_ip, &dns_param, cancel, deadline_fn)
    })?;
    let mut cache = DOH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let expires = now + Duration::from_secs(ttl_secs.clamp(MIN_TTL_SECS, MAX_TTL_SECS));
    cache_insert(
        &mut cache,
        key,
        ips.clone(),
        expires,
        now,
        MAX_CACHE_ENTRIES,
    );
    Ok(ips)
}

fn query_servers<T>(
    cancel: &HttpCancel,
    current_deadline: impl Fn() -> Instant,
    mut query: impl FnMut(Ipv4Addr, &dyn Fn() -> Instant) -> Result<T>,
) -> Result<T> {
    let mut last_err = anyhow!("all DoH resolvers failed");
    loop {
        let round_deadline = current_deadline();
        for server_ip in DOH_SERVERS {
            if cancel.is_cancelled() || Instant::now() >= current_deadline() {
                break;
            }
            match query(server_ip, &current_deadline) {
                Ok(val) => return Ok(val),
                Err(err) => last_err = err,
            }
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

fn query_single_server(
    server_ip: Ipv4Addr,
    dns_param: &str,
    cancel: &HttpCancel,
    current_deadline: &dyn Fn() -> Instant,
) -> Result<(Vec<IpAddr>, u64)> {
    let attempt_cap = Instant::now() + DOH_ATTEMPT_TIMEOUT;
    let step_end = || current_deadline().min(attempt_cap);
    let step_timeout = remaining_timeout(cancel, Some(step_end()), DOH_ATTEMPT_TIMEOUT)?;
    let tcp = connect_tcp(
        SocketAddr::new(IpAddr::V4(server_ip), 443),
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
    let sni = ServerName::IpAddress(PkiIpAddr::from(server_ip));
    let mut tls_conn = rustls::ClientConnection::new(tls_config()?, sni)?;
    while tls_conn.is_handshaking() {
        let end = step_end();
        timed.deadline = Some(end);
        remaining_timeout(cancel, Some(end), DOH_ATTEMPT_TIMEOUT)?;
        tls_conn.complete_io(&mut timed)?;
    }
    timed.deadline = Some(step_end());
    let mut stream = rustls::StreamOwned::new(tls_conn, timed);

    let req = format!(
        "GET /dns-query?dns={dns_param} HTTP/1.1\r\nHost: {server_ip}\r\nAccept: application/dns-message\r\nConnection: close\r\n\r\n"
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
    let mut body = Vec::with_capacity(512);
    match framing {
        DohFraming::Chunked => {
            let reader = ChunkedReader {
                inner: BufReader::new(chained),
                remaining: 0,
                need_crlf: false,
                eof: false,
            };
            reader
                .take(MAX_DOH_BODY_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
        }
        DohFraming::Fixed(len) => {
            chained.take(len as u64).read_to_end(&mut body)?;
            if body.len() != len {
                bail!("truncated DoH body");
            }
        }
        DohFraming::CloseDelimited => {
            chained
                .take(MAX_DOH_BODY_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
        }
    }
    if body.len() > MAX_DOH_BODY_BYTES {
        bail!("DoH body exceeds limit");
    }

    parse_dns_a_response(&body)
}

fn encode_dns_a_query(host: &str) -> Result<Vec<u8>> {
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

fn parse_dns_a_response(msg: &[u8]) -> Result<(Vec<IpAddr>, u64)> {
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
        CachedDns, DohFraming, HttpCancel, cache_insert, encode_dns_a_query, parse_dns_a_response,
        parse_doh_framing, query_servers, resolve_a_records, should_bypass_fake_ip,
    };
    use anyhow::anyhow;
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Mutex;
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
    fn encodes_and_parses_rfc1035_cname_plus_a_wire_messages() {
        let q = encode_dns_a_query("passport.bilibili.com").unwrap();
        assert_eq!(&q[..12], &[0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);

        let mut resp = Vec::new();
        // Header: QR=1, RD=1, RA=1, NOERROR, QD=1, AN=2
        resp.extend_from_slice(&[0, 0, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0]);
        resp.extend_from_slice(&q[12..]);
        // Answer 1: CNAME pointer 0xC00C -> "cdn.example"
        resp.extend_from_slice(&[
            0xC0, 0x0C, 0, 5, 0, 1, 0, 0, 1, 44, 0, 5, 3, b'c', b'd', b'n', 0,
        ]);
        // Answer 2: A pointer 0xC00C -> 14.17.92.73, TTL=45
        resp.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 45, 0, 4, 14, 17, 92, 73]);

        let (ips, ttl) = parse_dns_a_response(&resp).unwrap();
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(14, 17, 92, 73))]);
        assert_eq!(ttl, 45);
    }

    #[test]
    fn doh_cache_evicts_earliest_expiry_and_headers_reject_duplicates() {
        let now = Instant::now();
        let ip = vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))];
        let mut cache: BTreeMap<String, CachedDns> = BTreeMap::new();
        // "a.example" expires later than "z.example"; inserting a 3rd entry at cap=2 must evict "z.example".
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
        // Reject duplicate Content-Length (even identical), conflicting TE+CL, non-decimal, or gzip.
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

        // Pre-cancelled caller aborts immediately without probing network servers.
        let cancel = HttpCancel::default();
        cancel.cancel();
        assert!(
            resolve_a_records("uncached.example", &cancel, || now + Duration::from_secs(5))
                .is_err()
        );

        // Dynamic deadline extension from a joining waiter rescues an expiring DoH round.
        let shared_deadline = Mutex::new(Instant::now() + Duration::from_millis(50));
        let mut attempts = 0usize;
        let active_cancel = HttpCancel::default();
        let rescued = query_servers(
            &active_cancel,
            || *shared_deadline.lock().unwrap(),
            |_ip, deadline_fn| {
                attempts += 1;
                if attempts == 1 {
                    // Simulate a second waiter extending the shared deadline while attempt #1 fails.
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
}
