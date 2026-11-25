//! Per-task proxy for the supported metadata adapter. TLS stays end-to-end;
//! every CONNECT reauthorizes its Origin, DNS result and actual peer. No vault
//! credentials/private-origin grants enter this public-web adapter.
use crate::{
    plugin_v2::{
        files::random_name,
        ledger::{Activity, Ledger, Resource},
        network::{NetworkGrant, Origin, TransportKind, normalize_url},
    },
    vfs::{
        HttpCancel,
        http_client::{
            duplex::{self, Wire},
            raw::{self, RawRequest, RawState},
        },
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use iris_api::v2::ErrorCode;
use mio::{Events, Interest, Poll, Token, Waker};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct Proxy {
    pub url: String,
    state: Arc<State>,
}
struct State {
    cancel: HttpCancel,
    grant: NetworkGrant,
    ledger: Ledger,
    authorization: String,
    active: AtomicUsize,
    bytes: AtomicU64,
    https_seen: AtomicBool,
    failure: AtomicI32,
    activity: Activity,
}
impl Proxy {
    pub fn start(
        network: NetworkGrant,
        ledger: Ledger,
        cancel: &HttpCancel,
        activity: Activity,
        initial: &str,
    ) -> Result<Self, ErrorCode> {
        let initial = normalize_url(initial, TransportKind::Http)?;
        network.request(initial.as_str(), TransportKind::Http, None)?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|_| ErrorCode::IoError)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ErrorCode::IoError)?;
        let address = listener.local_addr().map_err(|_| ErrorCode::IoError)?;
        let token = random_name()?;
        let state = Arc::new(State {
            cancel: cancel.child().map_err(|_| ErrorCode::Cancelled)?,
            grant: network,
            ledger,
            authorization: format!("Basic {}", STANDARD.encode(format!("iris:{token}"))),
            active: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            https_seen: AtomicBool::new(initial.scheme() == "https"),
            failure: AtomicI32::new(0),
            activity,
        });
        let lease = state.activity.start();
        let server = state.clone();
        std::thread::Builder::new()
            .name("iris-tool-proxy".into())
            .spawn(move || {
                let _lease = lease;
                while !server.cancel.is_cancelled() {
                    match listener.accept() {
                        Ok((socket, peer)) if peer.ip().is_loopback() => {
                            if server
                                .active
                                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                    (n < 4).then_some(n + 1)
                                })
                                .is_err()
                            {
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            let reservation = server.ledger.reserve(
                                server.grant.owner(),
                                &[
                                    (Resource::Http, 1),
                                    (Resource::ControlHttp, 1),
                                    (Resource::HostBytes, 3 << 20),
                                ],
                            );
                            let Ok(reservation) = reservation else {
                                server.active.fetch_sub(1, Ordering::AcqRel);
                                std::thread::sleep(Duration::from_millis(5));
                                continue;
                            };
                            let worker = server.clone();
                            let activity = server.activity.start();
                            if std::thread::Builder::new()
                                .name("iris-tool-proxy-request".into())
                                .spawn(move || {
                                    let _lease = reservation;
                                    let _activity = activity;
                                    if let Err(error) = request(socket, &worker) {
                                        let _ = worker.failure.compare_exchange(
                                            0,
                                            error as i32,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        );
                                    }
                                    worker.active.fetch_sub(1, Ordering::AcqRel);
                                })
                                .is_err()
                            {
                                server.active.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|_| ErrorCode::Internal)?;
        Ok(Self {
            url: format!("http://iris:{token}@{address}"),
            state,
        })
    }
    pub fn failure(&self) -> Option<ErrorCode> {
        ErrorCode::from_i32(self.state.failure.load(Ordering::Acquire))
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.state.cancel.cancel();
    }
}
fn request(mut socket: TcpStream, state: &State) -> Result<(), ErrorCode> {
    socket.set_nonblocking(false).map_err(io)?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(io)?;
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(io)?;
    let cancel = state.cancel.child().map_err(|_| ErrorCode::Cancelled)?;
    let _socket = cancel.register(&socket).map_err(|_| ErrorCode::Cancelled)?;
    let mut bytes = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 32768 || Instant::now() >= end || cancel.is_cancelled() {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut byte = [0];
        socket.read_exact(&mut byte).map_err(io)?;
        bytes.push(byte[0]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    if !request
        .parse(&bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?
        .is_complete()
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let header = |name: &str| -> Result<Option<&[u8]>, ErrorCode> {
        let mut fields = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name));
        let value = fields.next().map(|h| h.value);
        if fields.next().is_some() {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(value)
    };
    if header("proxy-authorization")? != Some(state.authorization.as_bytes())
        || header("origin")?.is_some()
        || header("sec-fetch-site")?.is_some()
    {
        let _ = socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        return Err(ErrorCode::Denied);
    }
    if header("transfer-encoding")?.is_some()
        || header("upgrade")?.is_some()
        || header("expect")?.is_some()
    {
        return Err(ErrorCode::Unsupported);
    }
    let length = header("content-length")?
        .map(|v| {
            if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
                return Err(ErrorCode::InvalidArgument);
            }
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(ErrorCode::InvalidArgument)
        })
        .transpose()?
        .unwrap_or(0);
    if length > 1 << 20 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let target = request.path.ok_or(ErrorCode::InvalidArgument)?;
    let method = request.method.ok_or(ErrorCode::InvalidArgument)?;
    let url = if method == "CONNECT" {
        if length != 0 || target.contains(['/', '?', '#', '@', '\\']) {
            return Err(ErrorCode::Denied);
        }
        let url = normalize_url(&format!("https://{target}/"), TransportKind::Http)?;
        state.https_seen.store(true, Ordering::Release);
        url
    } else {
        let url = normalize_url(target, TransportKind::Http)?;
        if url.scheme() != "http"
            || !matches!(method, "GET" | "HEAD" | "POST")
            || state.https_seen.load(Ordering::Acquire)
        {
            return Err(ErrorCode::Denied);
        }
        url
    };
    let grant = state.grant.request_tracked(
        url.as_str(),
        TransportKind::Http,
        None,
        state.activity.clone(),
    )?;
    grant
        .cancel()
        .link(&cancel)
        .map_err(|_| ErrorCode::Cancelled)?;
    let expected_host = Origin::of(&url)?;
    if let Some(host) = header("host")? {
        let host = std::str::from_utf8(host).map_err(|_| ErrorCode::InvalidArgument)?;
        if Origin::of(&normalize_url(
            &format!("{}://{host}/", url.scheme()),
            TransportKind::Http,
        )?)? != expected_host
        {
            return Err(ErrorCode::Denied);
        }
    } else if method != "CONNECT" || request.version != Some(0) {
        // The pinned Python 3.10 urllib sends authenticated HTTP/1.0 CONNECT
        // without Host; the request-target is its sole checked authority.
        return Err(ErrorCode::InvalidArgument);
    }
    if method == "CONNECT" {
        let wire = Wire::tunnel(&url, grant, &cancel)?;
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .map_err(io)?;
        return tunnel(socket, wire, &cancel, state);
    }
    let mut body = vec![0; length];
    socket.read_exact(&mut body).map_err(io)?;
    let headers = request
        .headers
        .iter()
        .filter(|h| {
            !matches!(
                h.name.to_ascii_lowercase().as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "proxy-authorization"
                    | "proxy-connection"
            )
        })
        .map(|h| {
            (
                h.name.to_owned(),
                h.value.iter().map(|&b| char::from(b)).collect::<String>(),
            )
        })
        .collect();
    let mut raw = raw::open(
        RawRequest {
            method: method.into(),
            url,
            headers,
            follow_redirects: false,
            timeout: Duration::from_secs(30),
            cookies: None,
        },
        Some(&body),
        false,
        grant,
        cancel.clone(),
    )?;
    let RawState::Response(response) = &mut raw else {
        return Err(ErrorCode::Internal);
    };
    let mut head = format!(
        "HTTP/1.1 {} Proxy\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n",
        response.status
    )
    .into_bytes();
    for (name, value) in &response.headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "connection"
                | "transfer-encoding"
                | "content-length"
                | "proxy-authenticate"
                | "trailer"
        ) {
            continue;
        }
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        for character in value.chars() {
            head.push(u8::try_from(character).map_err(|_| ErrorCode::InvalidArgument)?);
        }
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    socket.write_all(&head).map_err(io)?;
    if method == "HEAD" {
        return Ok(());
    }
    let mut bytes = [0; 16384];
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let n = raw.read(&mut bytes, end)?;
        if n == 0 {
            break;
        }
        charge(state, n)?;
        write!(socket, "{n:x}\r\n").map_err(io)?;
        socket.write_all(&bytes[..n]).map_err(io)?;
        socket.write_all(b"\r\n").map_err(io)?;
    }
    socket.write_all(b"0\r\n\r\n").map_err(io)
}
fn charge(state: &State, bytes: usize) -> Result<(), ErrorCode> {
    if state.bytes.fetch_add(bytes as u64, Ordering::AcqRel) + bytes as u64 > 64 << 20 {
        state.cancel.cancel();
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(())
}
fn tunnel(
    socket: TcpStream,
    mut wire: Wire,
    cancel: &HttpCancel,
    state: &State,
) -> Result<(), ErrorCode> {
    socket.set_nonblocking(true).map_err(io)?;
    let mut client = mio::net::TcpStream::from_std(socket);
    wire.nonblocking().map_err(io)?;
    let mut poll = Poll::new().map_err(io)?;
    let interest = Interest::READABLE | Interest::WRITABLE;
    poll.registry()
        .register(&mut client, Token(0), interest)
        .map_err(io)?;
    wire.register(poll.registry(), Token(1), interest)
        .map_err(io)?;
    let _wake = duplex::cancellation_wake(
        cancel,
        Arc::new(Waker::new(poll.registry(), Token(2)).map_err(io)?),
    )?;
    let mut events = Events::with_capacity(4);
    let mut upstream = Pending::default();
    let mut downstream = Pending::default();
    let mut upstream_shutdown = false;
    let mut downstream_shutdown = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ErrorCode::Timeout);
        }
        let a = upstream.transfer(&mut client, &mut wire, state)?;
        let b = downstream.transfer(&mut wire, &mut client, state)?;
        if upstream.closed && upstream.bytes.is_empty() && !upstream_shutdown {
            match wire.shutdown_write() {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {}
                Err(e) => return Err(io(e)),
            }
            upstream_shutdown = true;
        }
        if downstream.closed && downstream.bytes.is_empty() && !downstream_shutdown {
            match client.shutdown(std::net::Shutdown::Write) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {}
                Err(e) => return Err(io(e)),
            }
            downstream_shutdown = true;
        }
        if upstream.closed
            && upstream.bytes.is_empty()
            && downstream.closed
            && downstream.bytes.is_empty()
        {
            return Ok(());
        }
        if !a && !b {
            match poll.poll(&mut events, Some(Duration::from_millis(100))) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io(e)),
            }
        }
    }
}
#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    offset: usize,
    closed: bool,
}
impl Pending {
    fn transfer(
        &mut self,
        from: &mut impl Read,
        to: &mut impl Write,
        state: &State,
    ) -> Result<bool, ErrorCode> {
        let mut progress = false;
        if self.bytes.is_empty() && !self.closed {
            let mut bytes = [0; 16384];
            match from.read(&mut bytes) {
                Ok(0) => self.closed = true,
                Ok(n) => {
                    charge(state, n)?;
                    self.bytes.extend_from_slice(&bytes[..n]);
                    progress = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(io(e)),
            }
        }
        if self.offset < self.bytes.len() {
            match to.write(&self.bytes[self.offset..]) {
                Ok(0) => return Err(ErrorCode::IoError),
                Ok(n) => {
                    self.offset += n;
                    progress = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(io(e)),
            }
            if self.offset == self.bytes.len() {
                self.bytes.clear();
                self.offset = 0;
            }
        }
        Ok(progress)
    }
}
fn io(_: std::io::Error) -> ErrorCode {
    ErrorCode::IoError
}
