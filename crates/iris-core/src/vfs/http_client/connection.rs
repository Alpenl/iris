//! Resolution, connection setup and cancellation shared by VFS and authenticated playback.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use mio::{Events, Interest, Poll, Token, Waker};
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
            if a == 0
                || v4.is_link_local()
                || v4.is_multicast()
                || a >= 240
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                || (a == 198 && (18..=19).contains(&b))
            {
                AddressKind::Forbidden
            } else if v4.is_loopback() {
                AddressKind::Loopback
            } else if v4.is_private() || (a == 100 && (64..128).contains(&b)) {
                AddressKind::Private
            } else {
                AddressKind::Public
            }
        }
        IpAddr::V6(v6) => {
            let parts = v6.segments();
            if v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || parts[0] & 0xffc0 == 0xfec0
                || v6 == std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
            {
                AddressKind::Forbidden
            } else if v6.is_loopback() {
                AddressKind::Loopback
            } else if parts[..6] == [0; 6]
                || parts[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
                || parts[..3] == [0x64, 0xff9b, 1]
                || parts[0] == 0x2002
                || parts[..2] == [0x2001, 0]
            {
                // Embedded destinations depend on tunnel configuration. Only
                // canonical IPv4-mapped addresses have native IPv4 semantics.
                AddressKind::Forbidden
            } else if v6.is_unique_local() {
                AddressKind::Private
            } else {
                AddressKind::Public
            }
        }
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

    pub(crate) fn source(&self) -> &Url {
        &self.source
    }

    fn permits(&self, url: &Url, addr: SocketAddr) -> bool {
        let ip = addr.ip().to_canonical();
        match address_kind(ip) {
            AddressKind::Forbidden => false,
            AddressKind::Loopback if addr.port() < 1024 => false,
            AddressKind::Public => true,
            AddressKind::Private | AddressKind::Loopback => {
                self.source.origin() == url.origin()
                    && self
                        .private_peers
                        .get()
                        .is_none_or(|peers| peers.contains(&ip))
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
    connects: BTreeMap<u64, Waker>,
}

impl HttpCancel {
    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        state.cancelled = true;
        for socket in state.sockets.values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        for wake in state.connects.values() {
            let _ = wake.wake();
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
    fn register_connect(&self, wake: Waker) -> Result<ConnectRegistration> {
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            bail!("HTTP request cancelled");
        }
        state.next_id += 1;
        let id = state.next_id;
        state.connects.insert(id, wake);
        Ok(ConnectRegistration(self.clone(), id))
    }
}

pub(crate) struct SocketRegistration(HttpCancel, u64);

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().sockets.remove(&self.1);
    }
}

struct ConnectRegistration(HttpCancel, u64);

impl Drop for ConnectRegistration {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().connects.remove(&self.1);
    }
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(v4)) => v4.is_loopback(),
        Some(Host::Ipv6(v6)) => address_kind(IpAddr::V6(v6)) == AddressKind::Loopback,
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.');
            d == "localhost" || d.ends_with(".localhost")
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

type LookupResult = std::result::Result<Vec<IpAddr>, String>;
type Lookup = Arc<(Mutex<Option<LookupResult>>, std::sync::Condvar)>;

fn resolve_system(
    host: &str,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<Vec<IpAddr>> {
    // The platform resolver preserves the hosts file, split DNS, and Bonjour.
    // Share one outstanding lookup per host and isolate its bounded workers
    // from numeric-address playback and TCP connection setup.
    static LOOKUPS: Mutex<BTreeMap<String, Lookup>> = Mutex::new(BTreeMap::new());
    let end = Instant::now() + remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?;
    let mut pending = LOOKUPS.lock().unwrap();
    let lookup = if let Some(lookup) = pending.get(host) {
        lookup.clone()
    } else {
        if pending.len() >= 8 {
            bail!("system DNS lookup limit reached");
        }
        let lookup = Arc::new((Mutex::new(None), std::sync::Condvar::new()));
        let worker = lookup.clone();
        let hostname = host.to_owned();
        std::thread::Builder::new()
            .name("iris-http-dns".into())
            .spawn(move || {
                let result = (hostname.as_str(), 0)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|addr| addr.ip()).collect())
                    .map_err(|err| err.to_string());
                *worker.0.lock().unwrap() = Some(result);
                worker.1.notify_all();
                LOOKUPS.lock().unwrap().remove(&hostname);
            })?;
        pending.insert(host.to_owned(), lookup.clone());
        lookup
    };
    drop(pending);

    let mut result = lookup.0.lock().unwrap();
    loop {
        let left = remaining_timeout(cancel, Some(end), Duration::from_millis(50))?;
        if let Some(result) = result.as_ref() {
            return result
                .clone()
                .map_err(|err| anyhow!("system DNS lookup failed: {err}"));
        }
        result = lookup.1.wait_timeout(result, left).unwrap().0;
    }
}

fn connect_tcp(
    addr: SocketAddr,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<TcpStream> {
    const SOCKET: Token = Token(0);
    const CANCEL: Token = Token(1);
    let end = Instant::now() + remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?;
    let mut poll = Poll::new()?;
    let _wake = cancel.register_connect(Waker::new(poll.registry(), CANCEL)?)?;
    let mut socket = mio::net::TcpStream::connect(addr)?;
    poll.registry()
        .register(&mut socket, SOCKET, Interest::WRITABLE)?;
    let mut events = Events::with_capacity(2);
    loop {
        let left = remaining_timeout(cancel, Some(end), CONNECT_TIMEOUT)?;
        match poll.poll(&mut events, Some(left)) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        }
        remaining_timeout(cancel, Some(end), CONNECT_TIMEOUT)?;
        if events.iter().any(|event| event.token() == SOCKET) {
            if let Some(err) = socket.take_error()? {
                return Err(err.into());
            }
            if socket.peer_addr().is_ok() {
                poll.registry().deregister(&mut socket)?;
                let tcp: TcpStream = socket.into();
                tcp.set_nonblocking(false)?;
                return Ok(tcp);
            }
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
        resolve_system(host, cancel, deadline)?
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect()
    };
    if addrs.is_empty() || addrs.iter().any(|&addr| !scope.permits(url, addr)) {
        bail!("blocked disallowed or empty HTTP destination");
    }
    let mut connected = None;
    for &addr in &addrs {
        if let Ok(tcp) = connect_tcp(addr, cancel, deadline) {
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
