//! Loopback-only HTTP transport for session capabilities. All blocking work is
//! bounded and off the UI; dropping Relay cancels without joining a worker.
use super::{
    range::Range,
    session::{Access, StreamSession},
};
use crate::{
    logging,
    plugin_v2::{
        actor::ReadResult,
        ledger::{Reservation, Resource},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
use mio::{Events, Interest, Poll, Token, Waker};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
const LISTENER: Token = Token(0);
const WAKE: Token = Token(1);
const HEADER_LIMIT: usize = 24 << 10;

pub struct BoundRelay {
    listener: mio::net::TcpListener,
    poll: Poll,
}
impl BoundRelay {
    pub fn bind() -> Result<Self, ErrorCode> {
        let listener =
            mio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).map_err(io)?;
        Ok(Self {
            listener,
            poll: Poll::new().map_err(io)?,
        })
    }
    pub fn authority(&self) -> Result<SocketAddr, ErrorCode> {
        self.listener.local_addr().map_err(io)
    }
    pub fn start(mut self, session: Arc<StreamSession>) -> Result<Relay, ErrorCode> {
        if session.authority() != self.authority()? {
            return Err(ErrorCode::Denied);
        }
        self.poll
            .registry()
            .register(&mut self.listener, LISTENER, Interest::READABLE)
            .map_err(io)?;
        let wake = Arc::new(Waker::new(self.poll.registry(), WAKE).map_err(io)?);
        let registration = session
            .cancellation()
            .register_shared_wake(wake.clone())
            .map_err(|_| ErrorCode::Cancelled)?;
        let service = Arc::new(Service {
            session,
            registry: self.poll.registry().try_clone().map_err(io)?,
            next: AtomicUsize::new(2),
            clients: Mutex::new(BTreeMap::new()),
            active: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            wake,
        });
        let server = service.clone();
        std::thread::Builder::new()
            .name("iris-v2-relay".into())
            .spawn(move || {
                let _registration = registration;
                serve_listener(&mut self, &server);
                server.session.cancel();
                server.done.store(true, Ordering::Release);
            })
            .map_err(io)?;
        Ok(Relay { service })
    }
}
pub struct Relay {
    service: Arc<Service>,
}
impl Relay {
    pub fn session(&self) -> &Arc<StreamSession> {
        &self.service.session
    }
    pub fn stop(&self) {
        self.service.session.cancel();
        let _ = self.service.wake.wake();
    }
    pub fn is_quiescent(&self) -> bool {
        self.service.done.load(Ordering::Acquire)
            && self.service.active.load(Ordering::Acquire) == 0
            // A worker/listener can publish completion just before its captured
            // Arc drops. Do not report release while it still owns the session.
            && Arc::strong_count(&self.service) == 1
            && self.service.session.is_quiescent()
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop();
    }
}
struct ClientState {
    cancel: HttpCancel,
    writable: Arc<(Mutex<u64>, Condvar)>,
}
struct Service {
    session: Arc<StreamSession>,
    registry: mio::Registry,
    next: AtomicUsize,
    clients: Mutex<BTreeMap<usize, ClientState>>,
    active: AtomicUsize,
    done: AtomicBool,
    wake: Arc<Waker>,
}
struct Worker {
    service: Arc<Service>,
    reservation: Option<Reservation>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.reservation.take();
        self.service.active.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Watch {
    service: Arc<Service>,
    stream: mio::net::TcpStream,
    token: usize,
    writable: Arc<(Mutex<u64>, Condvar)>,
    armed_writable: bool,
}
impl Watch {
    fn writable_gen(&self) -> u64 {
        *self.writable.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn wait_writable(&mut self, seen_gen: u64, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if let Err(err) = self.service.registry.reregister(
            &mut self.stream,
            Token(self.token),
            Interest::READABLE | Interest::WRITABLE,
        ) {
            if cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            return Err(io(err));
        }
        self.armed_writable = true;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let guard = self.writable.0.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == seen_gen {
            let _ = self
                .writable
                .1
                .wait_timeout(guard, Duration::from_millis(100));
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(())
    }
    fn disarm_writable(&mut self) {
        if self.armed_writable {
            self.armed_writable = false;
            let _ = self.service.registry.reregister(
                &mut self.stream,
                Token(self.token),
                Interest::READABLE,
            );
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.service
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
        let _ = self.service.registry.deregister(&mut self.stream);
    }
}
fn serve_listener(bound: &mut BoundRelay, service: &Arc<Service>) {
    let mut events = Events::with_capacity(32);
    let mut ready = false;
    while !service.session.cancellation().is_cancelled() {
        if ready {
            for _ in 0..128 {
                if service.session.cancellation().is_cancelled() {
                    break;
                }
                match bound.listener.accept() {
                    Ok((socket, peer)) if peer.ip().is_loopback() => {
                        let mut socket = TcpStream::from(socket);
                        if let Ok(worker) = reserve_worker(service) {
                            let _ = std::thread::Builder::new().name("iris-v2-range".into()).spawn(move || {
                                let service = worker.service.clone();
                                let Ok(cancel) = service.session.cancellation().child() else { return; };
                                let Ok(_socket) = cancel.register(&socket) else { return; };
                                let mut committed = false;
                                if let Err(error) = serve(&service, &mut socket, &cancel, &mut committed) {
                                    if matches!(error, ErrorCode::Cancelled | ErrorCode::RangeInvalid)
                                        || (committed && error == ErrorCode::IoError)
                                    {
                                        log::debug!(
                                            "plugin stream relay closed (committed={committed}): {error:?}"
                                        );
                                    } else {
                                        logging::event(
                                            logging::EventLevel::Warn,
                                            "stream",
                                            "relay_error",
                                            json!({
                                                "committed": committed,
                                                "error": format!("{error:?}"),
                                            }),
                                        );
                                    }
                                    if !committed {
                                        let session_dead =
                                            service.session.cancellation().is_cancelled();
                                        let status = match error {
                                            ErrorCode::RangeInvalid => 416,
                                            ErrorCode::StaleHandle => 410,
                                            ErrorCode::Denied => 403,
                                            ErrorCode::QuotaExceeded | ErrorCode::Backpressure => {
                                                503
                                            }
                                            ErrorCode::Cancelled if !session_dead => 503,
                                            ErrorCode::InvalidArgument => 400,
                                            _ => 502,
                                        };
                                        let _ = socket.set_nonblocking(false);
                                        let _ = socket
                                            .set_write_timeout(Some(Duration::from_millis(200)));
                                        let _ = write!(
                                            socket,
                                            "HTTP/1.1 {status} Error\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: 0\r\n\r\n"
                                        );
                                    }
                                }
                                cancel.cancel();
                            });
                        } else {
                            // Nonblocking small rejection; never wait for a
                            // malicious peer while admitting other clients.
                            let _ = socket.write_all(b"HTTP/1.1 503 Busy\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                        }
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        ready = false;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        ready = false;
                        break;
                    }
                }
            }
        }
        match bound
            .poll
            .poll(&mut events, ready.then_some(Duration::ZERO))
        {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(()) => (),
        }
        for event in &events {
            match event.token() {
                LISTENER => ready = true,
                WAKE => (),
                Token(id)
                    if event.is_readable()
                        || event.is_read_closed()
                        || event.is_write_closed()
                        || event.is_error() =>
                {
                    let client = service
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
                }
                Token(id) if event.is_writable() => {
                    let writable = service
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
                _ => (),
            }
        }
    }
}
fn reserve_worker(service: &Arc<Service>) -> Result<Worker, ErrorCode> {
    let factory = &service.session.factory;
    let reservation = factory.ledger().reserve(
        factory.owner(),
        &[
            (Resource::RelayWorker, 1),
            (Resource::HostBytes, (CHUNK_LIMIT * 2) as u64),
        ],
    )?;
    service
        .active
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 8).then_some(n + 1)
        })
        .map_err(|_| ErrorCode::Backpressure)?;
    Ok(Worker {
        service: service.clone(),
        reservation: Some(reservation),
    })
}
fn watch(
    service: &Arc<Service>,
    socket: &TcpStream,
    cancel: &HttpCancel,
) -> Result<Watch, ErrorCode> {
    let token = service
        .next
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
        .map_err(|_| ErrorCode::QuotaExceeded)?;
    let cloned = socket.try_clone().map_err(io)?;
    let mut stream = mio::net::TcpStream::from_std(cloned);
    let writable = Arc::new((Mutex::new(0u64), Condvar::new()));
    service
        .clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            token,
            ClientState {
                cancel: cancel.clone(),
                writable: writable.clone(),
            },
        );
    if let Err(error) = service
        .registry
        .register(&mut stream, Token(token), Interest::READABLE)
    {
        service
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&token);
        return Err(io(error));
    }
    Ok(Watch {
        service: service.clone(),
        stream,
        token,
        writable,
        armed_writable: false,
    })
}
fn request(socket: &mut TcpStream) -> Result<Vec<u8>, ErrorCode> {
    socket.set_nonblocking(false).map_err(io)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data = Vec::with_capacity(2048);
    let mut chunk = [0; 2048];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ErrorCode::Timeout)?;
        socket.set_read_timeout(Some(left)).map_err(io)?;
        let n = socket.read(&mut chunk).map_err(io)?;
        if n == 0 || data.len() + n > HEADER_LIMIT {
            return Err(ErrorCode::InvalidArgument);
        }
        let previous = data.len().saturating_sub(3);
        data.extend_from_slice(&chunk[..n]);
        if data[previous..].windows(4).any(|p| p == b"\r\n\r\n") {
            return Ok(data);
        }
    }
}
fn serve(
    service: &Arc<Service>,
    socket: &mut TcpStream,
    cancel: &HttpCancel,
    committed: &mut bool,
) -> Result<(), ErrorCode> {
    socket
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(io)?;
    let bytes = request(socket)?;
    let mut slots = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut slots);
    let used = match request
        .parse(&bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?
    {
        httparse::Status::Complete(n) => n,
        _ => return Err(ErrorCode::InvalidArgument),
    };
    if used != bytes.len()
        || !matches!(request.method, Some("GET" | "HEAD"))
        || request.version != Some(1)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut range = None;
    let mut host = false;
    let authority = service.session.authority().to_string();
    for h in request.headers {
        if h.name.eq_ignore_ascii_case("host") {
            if host || h.value != authority.as_bytes() {
                return Err(ErrorCode::Denied);
            }
            host = true;
        } else if h.name.eq_ignore_ascii_case("range") {
            if range.is_some() {
                return Err(ErrorCode::RangeInvalid);
            }
            range = Some(std::str::from_utf8(h.value).map_err(|_| ErrorCode::RangeInvalid)?);
        } else if h.name.eq_ignore_ascii_case("origin")
            || h.name.to_ascii_lowercase().starts_with("sec-fetch-")
            || h.name.eq_ignore_ascii_case("transfer-encoding")
            || h.name.eq_ignore_ascii_case("content-length") && h.value != b"0"
        {
            return Err(ErrorCode::Denied);
        }
    }
    if !host {
        return Err(ErrorCode::Denied);
    }
    // Switch to nonblocking mode with NO SO_SNDTIMEO before registering with mio
    // or writing any response bytes: on Windows Winsock, SO_SNDTIMEO can leave
    // bytes queued on WSAETIMEDOUT and cause duplicate-chunk stream corruption.
    socket.set_write_timeout(None).map_err(io)?;
    socket.set_read_timeout(None).map_err(io)?;
    socket.set_nonblocking(true).map_err(io)?;
    let mut watch = watch(service, socket, cancel)?;
    let access = service
        .session
        .open(request.path.ok_or(ErrorCode::InvalidArgument)?, cancel)?;
    let size = access.reader.size(&access.cancel)?;
    let mut range = match Range::parse(range, size) {
        Ok(range) => range,
        Err(error) => {
            *committed = true;
            let mut reply = String::from(
                "HTTP/1.1 416 Range Not Satisfiable\r\nConnection: close\r\nContent-Length: 0\r\n",
            );
            if let Some(size) = size {
                reply.push_str(&format!("Content-Range: bytes */{size}\r\n\r\n"));
            } else {
                reply.push_str("Content-Range: bytes */*\r\n\r\n");
            }
            write_all_cancellable(socket, reply.as_bytes(), &mut watch, cancel)?;
            return Err(error);
        }
    };
    let head = request.method == Some("HEAD");
    if !access.is_probed() && (head || range.offset > 0) {
        let prefix = super::prepare::probe_prefix(&access.reader, 64, &access.cancel)?;
        if super::prepare::adaptive(&prefix) {
            return Err(ErrorCode::Unsupported);
        }
        access.mark_probed();
    }
    if !head
        && range.len != Some(0)
        && let Some(mut stream) =
            access
                .reader
                .open_stream(range.offset, range.len, &access.cancel)?
    {
        if !access.is_probed() {
            if super::prepare::adaptive(&stream.first) {
                return Err(ErrorCode::Unsupported);
            }
            access.mark_probed();
        }
        if size.is_none() {
            if let Some(total) = stream.total {
                range.total = Some(total);
            }
            if let Some(rem) = stream.remaining {
                range.len = Some(rem);
            } else if stream.eof {
                let total = range
                    .offset
                    .checked_add(stream.first.len() as u64)
                    .ok_or(ErrorCode::RangeInvalid)?;
                if range.partial && stream.first.is_empty() {
                    return Err(ErrorCode::RangeInvalid);
                }
                range.total = Some(total);
                range.len = Some(stream.first.len() as u64);
            }
        }
        if access.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        *committed = true;
        write_response_headers(
            socket,
            &range,
            size.is_some() || range.total.is_some(),
            false,
            access.profile,
            &mut watch,
            &access.cancel,
        )?;
        return write_stream_body(socket, &access, range, &mut stream, &mut watch);
    }
    let first = if !head && range.len != Some(0) {
        let len = range
            .len
            .unwrap_or(CHUNK_LIMIT as u64)
            .min(CHUNK_LIMIT as u64) as u32;
        let first = access.reader.read_at(range.offset, len, &access.cancel)?;
        validate(&first, len, range.offset, range.total)?;
        if !access.is_probed() {
            if first.eof || first.blob.bytes.len() >= 64 {
                if super::prepare::adaptive(&first.blob.bytes) {
                    return Err(ErrorCode::Unsupported);
                }
            } else {
                let prefix = super::prepare::probe_prefix(&access.reader, 64, &access.cancel)?;
                if super::prepare::adaptive(&prefix) {
                    return Err(ErrorCode::Unsupported);
                }
            }
            access.mark_probed();
        }
        if size.is_none() {
            if first.eof {
                let total = range
                    .offset
                    .checked_add(first.blob.bytes.len() as u64)
                    .ok_or(ErrorCode::RangeInvalid)?;
                if range.partial && first.blob.bytes.is_empty() {
                    return Err(ErrorCode::RangeInvalid);
                }
                range.total = Some(total);
                range.len = Some(first.blob.bytes.len() as u64);
            } else if range.partial {
                range.len = Some(first.blob.bytes.len() as u64);
            }
        }
        Some(first)
    } else {
        None
    };
    if access.cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    *committed = true;
    write_response_headers(
        socket,
        &range,
        size.is_some(),
        head,
        access.profile,
        &mut watch,
        &access.cancel,
    )?;
    if head {
        return Ok(());
    }
    write_body(socket, access, range, first, &mut watch)
}
fn write_response_headers(
    socket: &mut TcpStream,
    range: &Range,
    accept_ranges: bool,
    head: bool,
    profile: Option<super::manifest::Profile>,
    watch: &mut Watch,
    cancel: &HttpCancel,
) -> Result<(), ErrorCode> {
    let mut header = format!(
        "HTTP/1.1 {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n",
        if range.partial {
            "206 Partial Content"
        } else {
            "200 OK"
        }
    );
    if let Some(size) = range.len {
        header.push_str(&format!("Content-Length: {size}\r\n"));
    } else if !head {
        header.push_str("Transfer-Encoding: chunked\r\n");
    }
    if accept_ranges {
        header.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(cr) = range.content_range() {
        header.push_str(&format!("Content-Range: {cr}\r\n"));
    }
    let mime = match profile {
        Some(super::manifest::Profile::Hls) => "application/vnd.apple.mpegurl",
        Some(super::manifest::Profile::Dash) => "application/dash+xml",
        None => "application/octet-stream",
    };
    header.push_str(&format!("Content-Type: {mime}\r\n\r\n"));
    write_all_cancellable(socket, header.as_bytes(), watch, cancel)
}
fn write_all_cancellable(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    watch: &mut Watch,
    cancel: &HttpCancel,
) -> Result<(), ErrorCode> {
    while !bytes.is_empty() {
        if cancel.is_cancelled() {
            watch.disarm_writable();
            return Err(ErrorCode::Cancelled);
        }
        let epoch = watch.writable_gen();
        match socket.write(bytes) {
            Ok(0) => {
                watch.disarm_writable();
                return Err(ErrorCode::IoError);
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(err) = watch.wait_writable(epoch, cancel) {
                    watch.disarm_writable();
                    return Err(err);
                }
            }
            Err(e) => {
                watch.disarm_writable();
                return Err(io(e));
            }
        }
    }
    watch.disarm_writable();
    Ok(())
}
fn write_chunk(
    socket: &mut TcpStream,
    bytes: &[u8],
    chunked: bool,
    watch: &mut Watch,
    cancel: &HttpCancel,
) -> Result<(), ErrorCode> {
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
fn write_stream_body(
    socket: &mut TcpStream,
    access: &Access,
    range: Range,
    stream: &mut super::reader::OpenedStream,
    watch: &mut Watch,
) -> Result<(), ErrorCode> {
    let chunked = range.len.is_none();
    let mut left = range.len;
    if !stream.first.is_empty() {
        let take = left.map_or(stream.first.len(), |n| stream.first.len().min(n as usize));
        write_chunk(
            socket,
            &stream.first[..take],
            chunked,
            watch,
            &access.cancel,
        )?;
        left = left.map(|n| n - take as u64);
    }
    if !stream.eof && left != Some(0) {
        let mut buf = vec![0u8; 64 << 10];
        while left != Some(0) {
            if access.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            let cap = left.map_or(buf.len(), |n| buf.len().min(n as usize));
            let n = stream.rest.read_chunk(&mut buf[..cap])?;
            if access.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if n == 0 {
                if left.is_some_and(|n| n != 0) {
                    return Err(ErrorCode::IoError);
                }
                break;
            }
            write_chunk(socket, &buf[..n], chunked, watch, &access.cancel)?;
            left = left.map(|rem| rem - n as u64);
        }
    }
    if chunked {
        write_all_cancellable(socket, b"0\r\n\r\n", watch, &access.cancel)?;
    }
    Ok(())
}
fn validate(read: &ReadResult, len: u32, offset: u64, total: Option<u64>) -> Result<(), ErrorCode> {
    let n = read.blob.bytes.len();
    if n > len as usize || n == 0 && !read.eof {
        return Err(ErrorCode::RangeInvalid);
    }
    let end = offset
        .checked_add(n as u64)
        .ok_or(ErrorCode::RangeInvalid)?;
    if total.is_some_and(|n| end > n || read.eof && end != n) {
        return Err(ErrorCode::RangeInvalid);
    }
    Ok(())
}
fn write_body(
    socket: &mut TcpStream,
    access: Access,
    range: Range,
    mut next: Option<ReadResult>,
    watch: &mut Watch,
) -> Result<(), ErrorCode> {
    let chunked = range.len.is_none();
    let mut offset = range.offset;
    let mut left = range.len;
    while left != Some(0) {
        let len = left.unwrap_or(CHUNK_LIMIT as u64).min(CHUNK_LIMIT as u64) as u32;
        let read = next
            .take()
            .map_or_else(|| access.reader.read_at(offset, len, &access.cancel), Ok)?;
        validate(&read, len, offset, range.total)?;
        if access.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let bytes = &read.blob.bytes;
        write_chunk(socket, bytes, chunked, watch, &access.cancel)?;
        offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or(ErrorCode::RangeInvalid)?;
        left = left.map(|n| n - bytes.len() as u64);
        if read.eof {
            if left.is_some_and(|n| n != 0) {
                return Err(ErrorCode::IoError);
            }
            break;
        }
    }
    if chunked {
        write_all_cancellable(socket, b"0\r\n\r\n", watch, &access.cancel)?;
    }
    Ok(())
}
fn io(error: std::io::Error) -> ErrorCode {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        ErrorCode::Timeout
    } else {
        ErrorCode::IoError
    }
}
