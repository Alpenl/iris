//! Resolution, connection setup and cancellation shared by VFS and authenticated playback.
pub(crate) mod doh;

use std::any::Any;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use mio::{Events, Interest, Poll, Token, Waker};
use rustls_pki_types::ServerName;
use url::{Host, Url};

use super::address::{AddressKind, address_kind};
use super::{CONNECT_TIMEOUT, IO_TIMEOUT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpClientError {
    Cancelled,
    Timeout,
    Denied,
}

impl std::fmt::Display for HttpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("HTTP request cancelled"),
            Self::Timeout => f.write_str("HTTP request deadline exceeded"),
            Self::Denied => f.write_str("HTTP destination denied"),
        }
    }
}

impl std::error::Error for HttpClientError {}

/// Optional per-connection authorization hook implemented by higher layers (e.g. plugin grants).
pub(crate) trait ConnectionPolicy: Send + Sync {
    fn check_url(&self, url: &Url) -> Result<()>;
    fn check_address(&self, url: &Url, addr: SocketAddr) -> Result<()>;
    fn cancel(&self) -> &HttpCancel;
    fn reserve_dns(&self) -> Result<Box<dyn Any + Send + Sync>>;
}

/// Private destinations are limited to the selected host's original addresses.
/// Public origins never acquire permission for a private destination via redirect or DNS.
pub(crate) struct NetworkScope {
    pub(super) source: Url,
    private_peers: OnceLock<Vec<IpAddr>>,
    policy: Option<Arc<dyn ConnectionPolicy>>,
}

impl NetworkScope {
    pub(crate) fn new(source: Url) -> Self {
        Self {
            source,
            private_peers: OnceLock::new(),
            policy: None,
        }
    }

    pub(crate) fn source(&self) -> &Url {
        &self.source
    }

    pub(crate) fn authorized(source: Url, policy: Arc<dyn ConnectionPolicy>) -> Result<Self> {
        policy.check_url(&source)?;
        Ok(Self {
            source,
            private_peers: OnceLock::new(),
            policy: Some(policy),
        })
    }

    fn permits(&self, url: &Url, addr: SocketAddr) -> bool {
        if let Some(policy) = &self.policy {
            return policy.check_address(url, addr).is_ok();
        }
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
    connects: BTreeMap<u64, Arc<Waker>>,
    children: Vec<std::sync::Weak<Mutex<CancelState>>>,
}

impl HttpCancel {
    /// Link an operation to a second cancellation authority without transferring
    /// ownership. Used for instance lifetime AND network-grant revocation.
    pub(crate) fn link(&self, child: &HttpCancel) -> Result<()> {
        if Arc::ptr_eq(&self.0, &child.0) {
            return Ok(());
        }
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            drop(state);
            child.cancel();
            bail!("operation cancelled");
        }
        state.children.retain(|w| w.strong_count() > 0);
        if !state
            .children
            .iter()
            .any(|w| w.ptr_eq(&Arc::downgrade(&child.0)))
        {
            state.children.push(Arc::downgrade(&child.0));
        }
        Ok(())
    }
    pub fn cancel(&self) {
        let children = {
            let mut state = self.0.lock().unwrap();
            state.cancelled = true;
            for socket in state.sockets.values() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            for wake in state.connects.values() {
                let _ = wake.wake();
            }
            std::mem::take(&mut state.children)
        };
        for child in children {
            if let Some(arc) = child.upgrade() {
                Self(arc).cancel();
            }
        }
    }

    pub(crate) fn child(&self) -> Result<Self> {
        let mut state = self.0.lock().unwrap();
        if state.cancelled {
            bail!("HTTP request cancelled");
        }
        state.children.retain(|w| w.strong_count() > 0);
        let child = Self::default();
        state.children.push(Arc::downgrade(&child.0));
        Ok(child)
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
        self.register_shared_wake(Arc::new(wake))
    }
    pub(crate) fn register_shared_wake(&self, wake: Arc<Waker>) -> Result<ConnectRegistration> {
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

pub(crate) struct ConnectRegistration(HttpCancel, u64);

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

pub(crate) struct TimedSocket {
    tcp: TcpStream,
    deadline: Option<Instant>,
    idle_timeout: Duration,
}

impl TimedSocket {
    pub(crate) fn into_tcp(self) -> TcpStream {
        self.tcp
    }

    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|d| {
                d.checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .map(|d| d.min(self.idle_timeout))
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

pub(crate) enum Transport {
    Plain(TimedSocket),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TimedSocket>>),
}

impl Transport {
    pub(crate) fn set_body_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        let socket = match self {
            Self::Plain(s) => s,
            Self::Tls(s) => &mut s.sock,
        };
        socket.deadline = deadline;
        socket.tcp.set_read_timeout(Some(socket.idle_timeout))
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
        return Err(
            anyhow::Error::new(HttpClientError::Cancelled).context("HTTP request cancelled")
        );
    }
    let left = deadline.map_or(limit, |d| {
        d.saturating_duration_since(Instant::now()).min(limit)
    });
    if left.is_zero() {
        return Err(
            anyhow::Error::new(HttpClientError::Timeout).context("HTTP request deadline exceeded")
        );
    }
    Ok(left)
}

type LookupResult = std::result::Result<Vec<IpAddr>, String>;
type Lookup = Arc<(Mutex<Option<LookupResult>>, std::sync::Condvar)>;

fn resolve_system(
    host: &str,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
    scope: &NetworkScope,
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
        let limit = 16;
        if pending.len() >= limit {
            bail!("system DNS lookup limit reached");
        }
        let dns_reservation = scope.policy.as_ref().map(|p| p.reserve_dns()).transpose()?;
        let lookup = Arc::new((Mutex::new(None), std::sync::Condvar::new()));
        let worker = lookup.clone();
        let hostname = host.to_owned();
        std::thread::Builder::new()
            .name("iris-http-dns".into())
            .spawn(move || {
                let _dns_reservation = dns_reservation;
                let worker_deadline = Instant::now() + CONNECT_TIMEOUT;
                let worker_cancel = HttpCancel::default();
                let result = (hostname.as_str(), 0)
                    .to_socket_addrs()
                    .map_err(|err| err.to_string())
                    .and_then(|addrs| {
                        let ips: Vec<_> = addrs.take(65).map(|addr| addr.ip()).collect();
                        if ips.len() > 64 {
                            Err("system DNS result exceeds address limit".into())
                        } else if doh::should_bypass_fake_ip(&hostname, &ips) {
                            doh::resolve_a_records(&hostname, &worker_cancel, worker_deadline)
                                .map_err(|err| format!("DoH fallback failed: {err}"))
                        } else {
                            Ok(ips)
                        }
                    });
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

fn filter_resolved_ips(ips: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    ips.into_iter()
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .collect()
}

pub(crate) fn connect_transport(
    url: &Url,
    scope: &NetworkScope,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
) -> Result<(Transport, SocketRegistration)> {
    connect_authorized(url, scope, cancel, deadline, true)
}

// A trusted HTTP proxy CONNECT keeps TLS end-to-end with the native tool. The
// original https Origin still drives DNS and peer authorization.
fn connect_authorized(
    url: &Url,
    scope: &NetworkScope,
    cancel: &HttpCancel,
    deadline: Option<Instant>,
    negotiate_tls: bool,
) -> Result<(Transport, SocketRegistration)> {
    if let Some(policy) = &scope.policy {
        policy.check_url(url)?;
        policy.cancel().link(cancel)?;
    }
    let connect_deadline = if scope.policy.is_some() {
        Some(Instant::now() + remaining_timeout(cancel, deadline, CONNECT_TIMEOUT)?)
    } else {
        deadline
    };
    let raw_host = url.host_str().ok_or_else(|| anyhow!("missing URL host"))?;
    let host = raw_host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_host);
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let ips = resolve_system(host, cancel, connect_deadline, scope)?;
        let ips = filter_resolved_ips(ips);
        ips.into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect()
    };
    if addrs.is_empty() || !addrs.iter().all(|&addr| scope.permits(url, addr)) {
        return Err(anyhow::Error::new(HttpClientError::Denied)
            .context("blocked disallowed or empty HTTP destination"));
    }
    let mut connected = None;
    for &addr in &addrs {
        if let Ok(tcp) = connect_tcp(addr, cancel, connect_deadline) {
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
            if !scope.permits(url, tcp.peer_addr()?) {
                bail!("HTTP peer outside authorized destination");
            }
            connected = Some(tcp);
            break;
        }
    }
    let tcp = connected.ok_or_else(|| anyhow!("HTTP connection failed"))?;
    let registration = cancel.register(&tcp)?;
    let idle_timeout = if scope.policy.is_some() {
        Duration::from_secs(30)
    } else {
        IO_TIMEOUT
    };
    let timeout = remaining_timeout(cancel, deadline, idle_timeout)?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let mut tcp = TimedSocket {
        tcp,
        deadline,
        idle_timeout,
    };
    if negotiate_tls && matches!(url.scheme(), "https" | "wss") {
        let sni = ServerName::try_from(host.to_string()).map_err(|_| anyhow!("bad SNI: {host}"))?;
        let mut conn = rustls::ClientConnection::new(tls_config()?, sni)?;
        let handshake_end =
            Instant::now() + remaining_timeout(cancel, connect_deadline, CONNECT_TIMEOUT)?;
        tcp.deadline = Some(handshake_end);
        while conn.is_handshaking() {
            remaining_timeout(cancel, Some(handshake_end), CONNECT_TIMEOUT)?;
            conn.complete_io(&mut tcp)?;
        }
        tcp.deadline = deadline;
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
        assert!(public.permits(&other, "93.184.216.34:9010".parse().unwrap()));
    }

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

    #[test]
    fn dual_stack_dns_filters_ipv6_link_local_while_blocking_pure_link_local_and_literals() {
        let bonjour = Url::parse("http://nas.local:5005/dav").unwrap();
        let scope = NetworkScope::new(bonjour.clone());
        let filtered = filter_resolved_ips([
            "192.168.1.42".parse().unwrap(),
            "fe80::1234:abcd".parse().unwrap(),
        ]);
        assert_eq!(filtered, vec!["192.168.1.42".parse::<IpAddr>().unwrap()]);
        assert!(
            filtered
                .iter()
                .all(|&ip| scope.permits(&bonjour, SocketAddr::new(ip, 5005)))
        );

        let only_link_local = filter_resolved_ips(["fe80::1".parse().unwrap()]);
        assert!(only_link_local.is_empty());
        assert!(validate_stream_url("http://[fe80::1]:5005/dav").is_err());

        let parent = HttpCancel::default();
        let child = parent.child().unwrap();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
        assert!(parent.child().is_err());
    }
}
