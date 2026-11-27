//! Loopback-only HTTP transport for session capabilities. All blocking work is
//! bounded and off the UI; dropping Relay cancels without joining a worker.
use super::{
    range::Range,
    session::{Access, StreamSession},
};
use crate::{
    actor::ReadResult,
    ledger::{Reservation, Resource},
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
use iris_base::net::http_client::HttpCancel;
use iris_base::net::loopback_http::{
    ClientHub, ClientWatch, HttpMethod, HttpRequestError, enter_nonblocking_body,
    parse_http_request, read_request, write_all_bounded, write_all_cancellable,
    write_chunk_cancellable,
};
use iris_base::rt::{ThreadRole, budget, spawn_thread};
use iris_core::logging;
use mio::{Events, Interest, Poll, Token, Waker};
use serde_json::json;
use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
const LISTENER: Token = Token(0);
const WAKE: Token = Token(1);
const HEADER_LIMIT: usize = 24 << 10;
const PRE_BODY_TIMEOUT: Duration = Duration::from_secs(5);

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
    pub fn start(self, session: Arc<StreamSession>) -> Result<Relay, ErrorCode> {
        self.start_with_timeout(session, PRE_BODY_TIMEOUT)
    }
    pub(super) fn start_with_timeout(
        mut self,
        session: Arc<StreamSession>,
        pre_body_timeout: Duration,
    ) -> Result<Relay, ErrorCode> {
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
            hub: Arc::new(ClientHub::new(
                self.poll.registry().try_clone().map_err(io)?,
                2,
            )),
            active: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            wake,
            pre_body_timeout,
        });
        let server = service.clone();
        spawn_thread(ThreadRole::PluginRelayListener, move || {
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
struct Service {
    session: Arc<StreamSession>,
    hub: Arc<ClientHub>,
    active: AtomicUsize,
    done: AtomicBool,
    wake: Arc<Waker>,
    pre_body_timeout: Duration,
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
                            // A second handle answers the client when no worker thread can
                            // start; it is dropped right after `spawn` so the worker's close
                            // still ends the connection.
                            let fallback = socket.try_clone();
                            let spawned = spawn_thread(ThreadRole::PluginRelayWorker, move || {
                                let service = worker.service.clone();
                                let Ok(cancel) = service.session.cancellation().child() else {
                                    return;
                                };
                                let Ok(_socket) = cancel.register(&socket) else {
                                    return;
                                };
                                let mut committed = false;
                                if let Err(error) =
                                    serve(&service, &mut socket, &cancel, &mut committed)
                                {
                                    if matches!(
                                        error,
                                        ErrorCode::Cancelled | ErrorCode::RangeInvalid
                                    ) || (committed && error == ErrorCode::IoError)
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
                            if let Err(error) = spawned {
                                log::warn!(
                                    "plugin stream relay could not start a range worker: {error}"
                                );
                                if let Ok(mut socket) = fallback {
                                    let _ = socket.write_all(b"HTTP/1.1 503 Busy\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                                }
                            }
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
                _ => service.hub.dispatch_event(event),
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
            (n < budget::PLUGIN_RELAY_WORKERS_PER_RELAY).then_some(n + 1)
        })
        .map_err(|_| ErrorCode::Backpressure)?;
    Ok(Worker {
        service: service.clone(),
        reservation: Some(reservation),
    })
}
fn map_watch_io(error: std::io::Error, cancel: &HttpCancel) -> ErrorCode {
    if cancel.is_cancelled() || error.kind() == std::io::ErrorKind::ConnectionAborted {
        ErrorCode::Cancelled
    } else if error.kind() == std::io::ErrorKind::InvalidData {
        ErrorCode::InvalidArgument
    } else {
        io(error)
    }
}
fn serve(
    service: &Arc<Service>,
    socket: &mut TcpStream,
    cancel: &HttpCancel,
    committed: &mut bool,
) -> Result<(), ErrorCode> {
    socket
        .set_write_timeout(Some(service.pre_body_timeout))
        .map_err(io)?;
    let bytes = read_request(socket, service.pre_body_timeout, HEADER_LIMIT)
        .map_err(|e| map_watch_io(e, cancel))?;
    let authority = service.session.authority().to_string();
    let parsed = match parse_http_request(&bytes, &authority) {
        Ok(parsed) => parsed,
        Err(err) => {
            *committed = true;
            let _ = socket.set_nonblocking(false);
            let _ = socket.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = write!(
                socket,
                "HTTP/1.1 {} {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: 0\r\n\r\n",
                err.status_code(),
                err.reason_phrase(),
            );
            return Err(match err {
                HttpRequestError::Forbidden => ErrorCode::Denied,
                HttpRequestError::RangeNotSatisfiable => ErrorCode::RangeInvalid,
                HttpRequestError::BadRequest
                | HttpRequestError::MethodNotAllowed
                | HttpRequestError::PayloadTooLarge => ErrorCode::InvalidArgument,
            });
        }
    };
    let range = parsed.range;
    // Switch to nonblocking mode with NO SO_SNDTIMEO before registering with mio
    // or writing any response bytes: on Windows Winsock, SO_SNDTIMEO can leave
    // bytes queued on WSAETIMEDOUT and cause duplicate-chunk stream corruption.
    enter_nonblocking_body(socket).map_err(io)?;
    let mut watch = service.hub.watch(socket, cancel.clone()).map_err(io)?;
    let access = service.session.open(parsed.path, cancel)?;
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
            write_all_bounded(
                socket,
                reply.as_bytes(),
                &mut watch,
                cancel,
                service.pre_body_timeout,
            )
            .map_err(|e| map_watch_io(e, cancel))?;
            return Err(error);
        }
    };
    let head = parsed.method == HttpMethod::Head;
    if !access.is_probed() && (head || range.offset > 0) {
        super::prepare::ensure_non_adaptive_prefix(&access.reader, &access.cancel)?;
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
            let resource_eof = (stream.eof && range.len.is_none())
                || size.or(stream.total) == Some(stream.first.len() as u64);
            if !resource_eof && super::prepare::adaptive_undecided(&stream.first) {
                super::prepare::ensure_non_adaptive_prefix(&access.reader, &access.cancel)?;
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
            service.pre_body_timeout,
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
            if super::prepare::adaptive(&first.blob.bytes) {
                return Err(ErrorCode::Unsupported);
            }
            if !first.eof && super::prepare::adaptive_undecided(&first.blob.bytes) {
                super::prepare::ensure_non_adaptive_prefix(&access.reader, &access.cancel)?;
            }
            access.mark_probed();
        }
        if size.is_none() && first.eof {
            let total = range
                .offset
                .checked_add(first.blob.bytes.len() as u64)
                .ok_or(ErrorCode::RangeInvalid)?;
            if range.partial && first.blob.bytes.is_empty() {
                return Err(ErrorCode::RangeInvalid);
            }
            range.total = Some(total);
            range.len = Some(first.blob.bytes.len() as u64);
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
        service.pre_body_timeout,
    )?;
    if head {
        return Ok(());
    }
    write_body(socket, access, range, first, &mut watch)
}
#[allow(clippy::too_many_arguments)]
fn write_response_headers(
    socket: &mut TcpStream,
    range: &Range,
    accept_ranges: bool,
    head: bool,
    profile: Option<super::manifest::Profile>,
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
    timeout: Duration,
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
        None => "application/octet-stream",
    };
    header.push_str(&format!("Content-Type: {mime}\r\n\r\n"));
    write_all_bounded(socket, header.as_bytes(), watch, cancel, timeout)
        .map_err(|e| map_watch_io(e, cancel))
}
fn write_chunk(
    socket: &mut TcpStream,
    bytes: &[u8],
    chunked: bool,
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
) -> Result<(), ErrorCode> {
    write_chunk_cancellable(socket, bytes, chunked, watch, cancel)
        .map_err(|e| map_watch_io(e, cancel))
}
fn write_stream_body(
    socket: &mut TcpStream,
    access: &Access,
    range: Range,
    stream: &mut super::reader::OpenedStream,
    watch: &mut ClientWatch,
) -> Result<(), ErrorCode> {
    debug_assert_eq!(socket.write_timeout().ok().flatten(), None);
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
        write_all_cancellable(socket, b"0\r\n\r\n", watch, &access.cancel)
            .map_err(|e| map_watch_io(e, &access.cancel))?;
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
    watch: &mut ClientWatch,
) -> Result<(), ErrorCode> {
    debug_assert_eq!(socket.write_timeout().ok().flatten(), None);
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
        write_all_cancellable(socket, b"0\r\n\r\n", watch, &access.cancel)
            .map_err(|e| map_watch_io(e, &access.cancel))?;
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
