//! Resolution, connection setup and cancellation shared by VFS and authenticated playback.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustls_pki_types::ServerName;
use url::{Host, Url};

use super::{CONNECT_TIMEOUT, IO_TIMEOUT};
#[derive(Clone, Copy, PartialEq, Eq)]
enum AddressKind {
    Public,
    Private,
    Loopback,
    Forbidden,
}

fn address_kind(ip: IpAddr) -> AddressKind {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if a == 0 || v4.is_link_local() || v4.is_multicast() || a >= 240 {
                AddressKind::Forbidden
            } else if v4.is_loopback() {
                AddressKind::Loopback
            } else if v4.is_private() || (a == 100 && (64..128).contains(&b)) {
                AddressKind::Private
            } else {
                AddressKind::Public
            }
        }
        IpAddr::V6(v6)
            if v6.is_unspecified() || v6.is_unicast_link_local() || v6.is_multicast() =>
        {
            AddressKind::Forbidden
        }
        IpAddr::V6(v6) if v6.is_loopback() => AddressKind::Loopback,
        IpAddr::V6(v6) if v6.is_unique_local() => AddressKind::Private,
        IpAddr::V6(_) => AddressKind::Public,
    }
}

/// Private destinations are limited to the selected host's original addresses.
/// Public origins never acquire permission for a private destination via redirect or DNS.
pub(crate) struct NetworkScope {
    pub(super) source: Url,
    private_peers: OnceLock<Vec<IpAddr>>,
}

impl NetworkScope {
    pub(crate) fn new(source: Url) -> Self {
        Self {
            source,
            private_peers: OnceLock::new(),
        }
    }

    fn permits(&self, url: &Url, addr: SocketAddr) -> bool {
        let ip = addr.ip().to_canonical();
        match address_kind(ip) {
            AddressKind::Forbidden => false,
            AddressKind::Loopback if addr.port() < 1024 => false,
            AddressKind::Public => true,
            AddressKind::Private if self.source.origin() != url.origin() => false,
            AddressKind::Private | AddressKind::Loopback => {
                self.source.host() == url.host()
                    && self.private_peers.get().map_or_else(
                        || self.source.origin() == url.origin(),
                        |peers| peers.contains(&ip),
                    )
            }
        }
    }
}

/// Cancels active HTTP operations, including blocked socket reads and TLS handshakes.
#[derive(Default, Clone)]
pub struct HttpCancel(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    next_id: u64,
    sockets: BTreeMap<u64, TcpStream>,
}

impl HttpCancel {
    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        state.cancelled = true;
        for socket in state.sockets.values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap().cancelled
    }

    pub(crate) fn register(&self, socket: &TcpStream) -> Result<SocketRegistration> {
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            bail!("HTTP request cancelled");
        }
        state.next_id += 1;
        let id = state.next_id;
        state.sockets.insert(id, socket.try_clone()?);
        Ok(SocketRegistration(self.clone(), id))
    }
}

pub(crate) struct SocketRegistration(HttpCancel, u64);

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().sockets.remove(&self.1);
    }
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(v4)) => v4.is_loopback(),
        Some(Host::Ipv6(v6)) => address_kind(IpAddr::V6(v6)) == AddressKind::Loopback,
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
    let parsed = Url::parse(trimmed).context("invalid stream URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "unsupported URL scheme '{}' (expected http or https)",
            parsed.scheme()
        );
    }
    match parsed.host() {
        Some(Host::Ipv4(v4)) if address_kind(IpAddr::V4(v4)) == AddressKind::Forbidden => {
            bail!("forbidden IPv4 stream host: {v4}");
        }
        Some(Host::Ipv6(v6)) if address_kind(IpAddr::V6(v6)) == AddressKind::Forbidden => {
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

fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: std::sync::LazyLock<std::result::Result<Arc<rustls::ClientConfig>, String>> =
        std::sync::LazyLock::new(|| {
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
        });
    CONFIG.clone().map_err(|e| anyhow!("{e}"))
}

pub(super) struct TimedSocket {
    tcp: TcpStream,
    deadline: Option<Instant>,
}

impl TimedSocket {
    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|d| {
                d.checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .map(|d| d.min(IO_TIMEOUT))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::TimedOut, "HTTP request deadline exceeded")
                    })
            })
            .transpose()
    }
}

impl Read for TimedSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(left) = self.remaining()? {
            self.tcp.set_read_timeout(Some(left))?;
        }
        self.tcp.read(buf)
    }
}

impl Write for TimedSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(left) = self.remaining()? {
            self.tcp.set_write_timeout(Some(left))?;
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

pub(super) enum Transport {
    Plain(TimedSocket),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TimedSocket>>),
}

impl Transport {
    pub(super) fn set_body_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        let socket = match self {
            Self::Plain(s) => s,
            Self::Tls(s) => &mut s.sock,
        };
        socket.deadline = deadline;
        socket.tcp.set_read_timeout(Some(IO_TIMEOUT))
    }
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

pub(super) fn remaining_timeout(
    cancel: &HttpCancel,
    deadline: Option<Instant>,
    limit: Duration,
) -> Result<Duration> {
    if cancel.is_cancelled() {
        bail!("HTTP request cancelled");
    }
    let left = deadline.map_or(limit, |d| {
        d.saturating_duration_since(Instant::now()).min(limit)
    });
    if left.is_zero() {
        bail!("HTTP request deadline exceeded");
    }
    Ok(left)
}

// The OS resolver and blocking connect cannot be interrupted portably. Bound the
// outstanding operations and release their caller immediately on cancellation.
fn network_setup<T: Send + 'static>(
    cancel: &HttpCancel,
    deadline: Option<Instant>,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let timeout = remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?;
    let end = Instant::now() + timeout;
    ACTIVE
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < 32).then_some(n + 1)
        })
        .map_err(|_| anyhow!("HTTP connection setup limit reached"))?;
    let slot = Slot;
    let (tx, rx) = sync_channel(1);
    std::thread::Builder::new()
        .name("iris-http-connect".into())
        .spawn(move || {
            let _slot = slot;
            let _ = tx.send(work());
        })?;
    loop {
        let left = remaining_timeout(cancel, Some(end), Duration::from_millis(50))?;
        match rx.recv_timeout(left) {
            Ok(result) => return Ok(result?),
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => bail!("HTTP setup interrupted"),
        }
    }
}

pub(super) fn connect_transport(
    url: &Url,
    scope: &NetworkScope,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<(Transport, SocketRegistration)> {
    let raw_host = url.host_str().ok_or_else(|| anyhow!("missing URL host"))?;
    let host = raw_host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_host);
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let host = host.to_owned();
        network_setup(cancel, deadline, move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect)
        })?
    };
    if addrs.is_empty() || addrs.iter().any(|&addr| !scope.permits(url, addr)) {
        bail!("blocked disallowed or empty HTTP destination");
    }
    let mut connected = None;
    for &addr in &addrs {
        let timeout = remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?;
        if let Ok(tcp) = network_setup(cancel, deadline, move || {
            TcpStream::connect_timeout(&addr, timeout)
        }) {
            if scope.source.origin() == url.origin() {
                scope.private_peers.get_or_init(|| {
                    let kind = address_kind(addr.ip());
                    if kind == AddressKind::Public {
                        return Vec::new();
                    }
                    addrs
                        .iter()
                        .map(|a| a.ip().to_canonical())
                        .filter(|ip| address_kind(*ip) == kind)
                        .collect()
                });
            }
            if !scope.permits(url, addr) {
                bail!("HTTP peer outside authorized destination");
            }
            connected = Some(tcp);
            break;
        }
    }
    let tcp = connected.ok_or_else(|| anyhow!("HTTP connection failed"))?;
    let registration = cancel.register(&tcp)?;
    let timeout = remaining_timeout(cancel, deadline, IO_TIMEOUT)?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let tcp = TimedSocket { tcp, deadline };
    if url.scheme() == "https" {
        let sni = ServerName::try_from(host.to_string()).map_err(|_| anyhow!("bad SNI: {host}"))?;
        let conn = rustls::ClientConnection::new(tls_config()?, sni)?;
        Ok((
            Transport::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))),
            registration,
        ))
    } else {
        Ok((Transport::Plain(tcp), registration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_policy_applies_ports_and_pins_private_redirects() {
        let selected = Url::parse("http://nas.example:9010/video").unwrap();
        let scope = NetworkScope::new(selected.clone());
        for low in ["127.0.0.1:80", "[::1]:443", "[::ffff:127.0.0.1]:100"] {
            assert!(!scope.permits(&selected, low.parse().unwrap()));
        }
        for high in [
            "127.0.0.1:9010",
            "[::1]:9010",
            "192.168.1.2:9010",
            "[fd00::1]:9010",
        ] {
            assert!(scope.permits(&selected, high.parse().unwrap()));
        }
        scope
            .private_peers
            .set(vec!["192.168.1.2".parse().unwrap()])
            .unwrap();
        let same_host = selected.join("/elsewhere").unwrap();
        assert!(scope.permits(&same_host, "192.168.1.2:9010".parse().unwrap()));
        let other_port = Url::parse("http://nas.example:9011/file").unwrap();
        assert!(!scope.permits(&other_port, "192.168.1.2:9011".parse().unwrap()));
        let other = Url::parse("http://other.example:9010/file").unwrap();
        for private in [
            "10.1.2.3:9010",
            "192.168.1.2:9010",
            "100.64.0.1:9010",
            "[fd00::1]:9010",
            "[::ffff:10.0.0.1]:9010",
        ] {
            assert!(!scope.permits(&other, private.parse().unwrap()));
        }
        assert!(!scope.permits(&same_host, "192.168.1.3:9010".parse().unwrap()));
        let public = NetworkScope::new(selected.clone());
        public.private_peers.set(vec![]).unwrap();
        for denied in [
            "127.0.0.1:9010",
            "[::1]:9010",
            "[::ffff:127.0.0.1]:9010",
            "169.254.1.1:9010",
            "[fe80::1]:9010",
        ] {
            let addr = denied.parse().unwrap();
            assert!(!public.permits(&other, addr));
            assert!(!public.permits(&selected, addr));
        }
        assert!(!public.permits(&selected, "10.0.0.1:9010".parse().unwrap()));
        assert!(public.permits(&other, "203.0.113.1:9010".parse().unwrap()));
    }
}

#[cfg(test)]
#[test]
fn cancelling_registered_socket_wakes_the_reader() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut reader = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_sender, _) = listener.accept().unwrap();
    let cancel = HttpCancel::default();
    let registration = cancel.register(&reader).unwrap();
    let (done, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _registration = registration;
        let value = reader.read(&mut [0]);
        done.send(value).unwrap();
    });
    cancel.cancel();
    assert!(!matches!(result.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(n) if n > 0));
    worker.join().unwrap();
}
