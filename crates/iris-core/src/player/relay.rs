//! Authenticated playback transport. FFmpeg's HTTP `headers` option is also used
//! for redirects and inherited by HLS child contexts. Keep credentials in Iris and
//! give libmpv encrypted, playback-scoped resource URLs instead.

mod playlist;
mod transfer;

use transfer::{copy_stream, ensure_identity, valid_range};

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mio::{Events, Interest, Poll, Token, Waker};
use playlist::{is_hls, rewrite_playlist};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use url::Url;

use crate::loopback_http::{
    ClientHub, is_adaptive_manifest, is_adaptive_prefix_undecided, read_request,
};
use crate::vfs::http_client::{
    HttpCancel, NetworkScope, Request, open_stream, validate_stream_url,
};

use super::model::fetch_subtitle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_URL: usize = 2800;
const MAX_REQUEST: usize = 24 * 1024;
const MAX_PLAYLIST: usize = 4 * 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const RELAY_WORKERS: usize = 8;
const GLOBAL_WORKERS: usize = 32;
const SLOT_WAIT: Duration = Duration::from_secs(1);
static ACTIVE_WORKERS: Mutex<usize> = Mutex::new(0);
static WORKER_AVAILABLE: Condvar = Condvar::new();

#[derive(Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    Media,
    Playlist,
    Subtitle,
}

impl RouteKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Media => "m",
            Self::Playlist => "p",
            Self::Subtitle => "s",
        }
    }
}

/// Each token authorizes one exact URL. No mutable playlist index is retained for live streams.
struct Routes {
    key: LessSafeKey,
    nonce: AtomicU64,
    authority: SocketAddr,
}

impl Routes {
    fn new(authority: SocketAddr) -> Result<Self> {
        let mut bytes = [0; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| anyhow!("could not generate playback key"))?;
        let key = UnboundKey::new(&CHACHA20_POLY1305, &bytes)
            .map_err(|_| anyhow!("invalid playback key"))?;
        Ok(Self {
            key: LessSafeKey::new(key),
            nonce: AtomicU64::new(0),
            authority,
        })
    }

    fn seal(&self, url: &Url, kind: RouteKind, fallback_ext: &str) -> Result<String> {
        ensure!(url.as_str().len() <= MAX_URL, "media URL exceeds limit");
        // FFmpeg checks the suffix of HLS segments. Expose only known format hints.
        let ext = if kind == RouteKind::Playlist {
            "m3u8"
        } else {
            url.path_segments()
                .and_then(|mut parts| parts.next_back())
                .and_then(|name| name.rsplit_once('.'))
                .and_then(|(_, suffix)| {
                    [
                        "mp4", "m4v", "m4a", "m4s", "cmfv", "cmfa", "ts", "m2ts", "aac", "ac3",
                        "ec3", "mp3", "vtt", "webvtt", "srt", "ass", "ssa", "webm", "mkv", "mov",
                        "ogg", "flac", "wav", "jpg", "jpeg", "png", "heic", "avif", "webp", "key",
                    ]
                    .into_iter()
                    .find(|ext| ext.eq_ignore_ascii_case(suffix))
                })
                .unwrap_or(fallback_ext)
        };
        let sequence = self
            .nonce
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| anyhow!("exhausted playback resource identifiers"))?;
        let mut iv = [0; 12];
        iv[4..].copy_from_slice(&sequence.to_be_bytes());
        let mut sealed = url.as_str().as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(iv),
                Aad::from(format!("{}:{ext}", kind.tag()).as_bytes()),
                &mut sealed,
            )
            .map_err(|_| anyhow!("cannot authorize playback resource"))?;
        Ok(format!(
            "http://{}/{}/{sequence:x}/{}.{ext}",
            self.authority,
            kind.tag(),
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    fn open(&self, path: &str) -> Result<(Url, RouteKind)> {
        let (tag, tail) = path
            .strip_prefix('/')
            .and_then(|p| p.split_once('/'))
            .context("invalid playback route")?;
        let kind = match tag {
            "m" => RouteKind::Media,
            "p" => RouteKind::Playlist,
            "s" => RouteKind::Subtitle,
            _ => bail!("invalid playback route kind"),
        };
        let (sequence, payload) = tail.split_once('/').context("invalid playback route")?;
        let (payload, ext) = payload.rsplit_once('.').context("invalid playback route")?;
        let mut iv = [0; 12];
        iv[4..].copy_from_slice(&u64::from_str_radix(sequence, 16)?.to_be_bytes());
        let mut bytes = URL_SAFE_NO_PAD.decode(payload)?;
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(iv),
                Aad::from(format!("{tag}:{ext}").as_bytes()),
                &mut bytes,
            )
            .map_err(|_| anyhow!("invalid playback capability"))?;
        Ok((validate_stream_url(std::str::from_utf8(plain)?)?, kind))
    }
}

pub(super) struct Service {
    routes: Routes,
    scope: NetworkScope,
    headers: BTreeMap<String, String>,
    cancel: HttpCancel,
    workers: AtomicUsize,
    hub: Arc<ClientHub>,
    wake: Waker,
}

pub(super) struct PlaybackRelay {
    service: Arc<Service>,
    listener: Option<thread::JoinHandle<()>>,
}

impl PlaybackRelay {
    pub(super) fn start(url: Url, headers: BTreeMap<String, String>) -> Result<Self> {
        const LISTENER: Token = Token(0);
        const WAKE: Token = Token(1);
        let mut listener =
            mio::net::TcpListener::bind(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)))?;
        let mut poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let wake = Waker::new(poll.registry(), WAKE)?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        let service = Arc::new(Service {
            routes: Routes::new(listener.local_addr()?)?,
            scope: NetworkScope::new(url),
            headers,
            cancel: HttpCancel::default(),
            workers: AtomicUsize::new(0),
            hub: Arc::new(ClientHub::new(registry, 2)),
            wake,
        });
        let server = Arc::clone(&service);
        let listener = thread::Builder::new()
            .name("iris-media-relay".into())
            .spawn(move || {
                let mut events = Events::with_capacity(16);
                let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
                let mut waiting: Option<(TcpStream, Instant)> = None;
                let mut accept_ready = false;
                while !server.cancel.is_cancelled() {
                    if let Some((socket, deadline)) = waiting.take() {
                        if let Some(worker) = server.try_reserve_worker() {
                            workers.retain(|w| !w.is_finished());
                            if let Some(handle) = server.spawn_worker(worker, socket) {
                                workers.push(handle);
                            }
                        } else if Instant::now() >= deadline {
                            reject_busy(socket);
                        } else {
                            waiting = Some((socket, deadline));
                        }
                    }
                    if waiting.is_none() && accept_ready {
                        while !server.cancel.is_cancelled() {
                            match listener.accept() {
                                Ok((socket, _)) => {
                                    let socket = TcpStream::from(socket);
                                    workers.retain(|w| !w.is_finished());
                                    if let Some(worker) = server.try_reserve_worker() {
                                        if let Some(handle) = server.spawn_worker(worker, socket) {
                                            workers.push(handle);
                                        }
                                    } else {
                                        waiting = Some((socket, Instant::now() + SLOT_WAIT));
                                        break;
                                    }
                                }
                                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                                    accept_ready = false;
                                    break;
                                }
                                Err(err)
                                    if matches!(
                                        err.kind(),
                                        io::ErrorKind::Interrupted
                                            | io::ErrorKind::ConnectionReset
                                            | io::ErrorKind::ConnectionAborted
                                    ) =>
                                {
                                    continue;
                                }
                                Err(err) => {
                                    log::warn!("relay accept failed: {err}");
                                    break;
                                }
                            }
                        }
                    }
                    let timeout = if let Some((_, deadline)) = waiting {
                        Some(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(10)),
                        )
                    } else if accept_ready {
                        Some(Duration::from_millis(20))
                    } else {
                        None
                    };
                    match poll.poll(&mut events, timeout) {
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                        Ok(()) => (),
                    }
                    for event in &events {
                        match event.token() {
                            LISTENER => accept_ready = true,
                            WAKE => (),
                            _ => server.hub.dispatch_event(event),
                        }
                    }
                }
                server.cancel.cancel();
                if let Some((socket, _)) = waiting {
                    drop(socket);
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Self {
            service,
            listener: Some(listener),
        })
    }

    pub(super) fn url(&self, url: &Url) -> Result<String> {
        self.service.routes.seal(url, RouteKind::Media, "media")
    }

    pub(super) fn subtitle_url(&self, url: &Url) -> Result<String> {
        self.service.routes.seal(url, RouteKind::Subtitle, "srt")
    }

    pub(super) fn service(&self) -> Arc<Service> {
        self.service.clone()
    }

    pub(super) fn cancel(&self) {
        self.service.cancel.cancel();
        // Synchronize with the admission check before waking the listener.
        let _workers = ACTIVE_WORKERS.lock().unwrap();
        WORKER_AVAILABLE.notify_all();
        let _ = self.service.wake.wake();
    }
}

impl Drop for PlaybackRelay {
    fn drop(&mut self) {
        self.cancel();
        if let Some(listener) = self.listener.take() {
            if listener.is_finished() {
                let _ = listener.join();
            } else {
                let handle = Arc::new(Mutex::new(Some(listener)));
                let bg = Arc::clone(&handle);
                if thread::Builder::new()
                    .name("iris-relay-reap".into())
                    .spawn(move || {
                        if let Some(h) = bg.lock().unwrap().take() {
                            let _ = h.join();
                        }
                    })
                    .is_err()
                    && let Some(h) = handle.lock().unwrap().take()
                {
                    let _ = h.join();
                }
            }
        }
    }
}

struct Worker(Arc<Service>);

impl Drop for Worker {
    fn drop(&mut self) {
        let mut active = ACTIVE_WORKERS.lock().unwrap();
        self.0.workers.fetch_sub(1, Ordering::Relaxed);
        *active -= 1;
        WORKER_AVAILABLE.notify_all();
        let _ = self.0.wake.wake();
    }
}

fn reject_busy(mut socket: TcpStream) {
    // Drain the ordinary small request before replying so closing the
    // socket does not discard the response as an unread-data reset.
    let _ = read_request(&mut socket, Duration::from_millis(100), MAX_REQUEST);
    let _ = socket.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = socket.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    );
}

impl Service {
    fn try_reserve_worker(self: &Arc<Self>) -> Option<Worker> {
        let mut active = ACTIVE_WORKERS.lock().unwrap();
        if self.cancel.is_cancelled()
            || *active >= GLOBAL_WORKERS
            || self.workers.load(Ordering::Relaxed) >= RELAY_WORKERS
        {
            return None;
        }
        *active += 1;
        self.workers.fetch_add(1, Ordering::Relaxed);
        Some(Worker(Arc::clone(self)))
    }

    fn spawn_worker(
        self: &Arc<Self>,
        worker: Worker,
        mut socket: TcpStream,
    ) -> Option<thread::JoinHandle<()>> {
        thread::Builder::new().name("iris-media-request".into()).spawn(move || {
            let service = &worker.0;
            let Ok(_registration) = service.cancel.register(&socket) else { return };
            let mut committed = false;
            if service.serve(&mut socket, &mut committed).is_err() && !committed {
                let _ = socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            }
        }).ok()
    }

    fn serve(&self, socket: &mut TcpStream, committed: &mut bool) -> Result<()> {
        socket.set_write_timeout(Some(CLIENT_TIMEOUT))?;
        let raw = read_request(socket, CLIENT_TIMEOUT, MAX_REQUEST)?;
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut slots);
        ensure!(
            request.parse(&raw)?.is_complete(),
            "incomplete relay request"
        );
        let method = request.method.context("missing request method")?;
        ensure!(matches!(method, "GET" | "HEAD"), "unsupported relay method");
        let host = self.routes.authority.to_string();
        let mut host_seen = false;
        let mut range = None;
        for h in request.headers {
            if h.name.eq_ignore_ascii_case("host") {
                ensure!(
                    !host_seen && h.value == host.as_bytes(),
                    "invalid relay authority"
                );
                host_seen = true;
            } else if h.name.eq_ignore_ascii_case("origin")
                || h.name.to_ascii_lowercase().starts_with("sec-fetch-")
            {
                bail!("browser access to playback relay denied");
            } else if h.name.eq_ignore_ascii_case("range") {
                let value = std::str::from_utf8(h.value)?;
                ensure!(range.is_none() && valid_range(value), "invalid relay range");
                range = Some(value);
            }
        }
        ensure!(host_seen, "missing relay authority");
        let (url, kind) = self
            .routes
            .open(request.path.context("missing playback route")?)?;
        let req_cancel = self.cancel.child()?;
        let mut client_watch = self.hub.watch(socket, req_cancel.clone())?;
        if kind == RouteKind::Subtitle {
            // Remote attachments share the media's address policy even when
            // their origin has no access to the media request headers.
            let empty = BTreeMap::new();
            let source = self.scope.source();
            let headers = if source.origin() == url.origin() {
                &self.headers
            } else {
                &empty
            };
            let credentials = if source.origin() == url.origin()
                && url.username().is_empty()
                && url.password().is_none()
            {
                source
            } else {
                &url
            };
            let text = fetch_subtitle(&url, headers, credentials, &self.scope, &req_cancel)?;
            *committed = true;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
                text.len()
            )?;
            if method == "GET" {
                socket.write_all(text.as_bytes())?;
            }
            return Ok(());
        }
        let playlist = kind == RouteKind::Playlist;
        let deadline = playlist.then(|| Instant::now() + REQUEST_TIMEOUT);
        let mut response = self.open(
            url,
            method,
            if playlist { None } else { range },
            deadline,
            &req_cancel,
        )?;
        if !matches!(response.status, 200 | 206) {
            *committed = true;
            write!(
                socket,
                "HTTP/1.1 {} Upstream response\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                response.status
            )?;
            return Ok(());
        }
        ensure_identity(&response)?;

        if playlist && method == "GET" {
            ensure!(response.status == 200, "incomplete HLS playlist");
            let mut data = Vec::new();
            response
                .body()
                .take(MAX_PLAYLIST as u64 + 1)
                .read_to_end(&mut data)?;
            ensure!(
                data.len() <= MAX_PLAYLIST,
                "HLS playlist exceeds byte limit"
            );
            let text = rewrite_playlist(std::str::from_utf8(&data)?, &response.url, &self.routes)?;
            *committed = true;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
                text.len()
            )?;
            socket.write_all(text.as_bytes())?;
            return Ok(());
        }

        let mut sniffed = Vec::new();
        let mut deferred_err = None;
        if !playlist && method == "GET" && range.is_none_or(|r| r.starts_with("bytes=0-")) {
            let mut buf = [0u8; 64];
            let mut used = 0;
            while used < buf.len() && is_adaptive_prefix_undecided(&buf[..used]) {
                match response.body().read(&mut buf[used..]) {
                    Ok(0) => break,
                    Ok(n) => used += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if used == 0 => return Err(e.into()),
                    Err(e) => {
                        deferred_err = Some(e);
                        break;
                    }
                }
            }
            ensure!(
                !is_adaptive_manifest(&buf[..used]),
                "unexpected adaptive manifest on media route"
            );
            sniffed.extend_from_slice(&buf[..used]);
        }
        let chunked = method == "GET" && !response.headers.contains_key("content-length");
        *committed = true;
        write!(
            socket,
            "HTTP/1.1 {} OK\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Type: application/octet-stream\r\n",
            response.status
        )?;
        for name in ["content-length", "content-range", "accept-ranges"] {
            if playlist {
                continue;
            }
            if let Some(value) = response.headers.get(name) {
                write!(socket, "{name}: {value}\r\n")?;
            }
        }
        if chunked {
            socket.write_all(b"Transfer-Encoding: chunked\r\n")?;
        }
        socket.write_all(b"\r\n")?;
        if let Some(err) = deferred_err {
            if !sniffed.is_empty() {
                if chunked {
                    write!(socket, "{:x}\r\n", sniffed.len())?;
                    socket.write_all(&sniffed)?;
                    socket.write_all(b"\r\n")?;
                } else {
                    socket.write_all(&sniffed)?;
                }
            }
            return Err(err.into());
        }
        if method == "GET" {
            // Switch to nonblocking writes with NO SO_SNDTIMEO before streaming
            // body bytes so long pauses or full demuxer caches never time out or
            // trigger Winsock SO_SNDTIMEO partial-write hazards.
            socket.set_write_timeout(None)?;
            socket.set_nonblocking(true)?;
            let mut combined = io::Cursor::new(sniffed).chain(response.body());
            copy_stream(
                &mut combined,
                socket,
                chunked,
                &mut client_watch,
                &req_cancel,
            )?;
        }
        Ok(())
    }

    fn open(
        &self,
        url: Url,
        method: &str,
        range: Option<&str>,
        deadline: Option<Instant>,
        cancel: &HttpCancel,
    ) -> Result<crate::vfs::http_client::HttpStream> {
        open_stream(
            Request {
                method,
                url,
                headers: &self.headers,
                body: None,
                range,
            },
            &self.scope,
            cancel,
            deadline,
        )
    }

    pub(super) fn playlist_url(&self, candidate: &str) -> Result<Option<String>> {
        let url = Url::parse(candidate)?;
        if url.port() != Some(self.routes.authority.port()) || url.host_str() != Some("127.0.0.1") {
            return Ok(None);
        }
        let Ok((source, RouteKind::Media)) = self.routes.open(url.path()) else {
            return Ok(None);
        };
        if &source != self.scope.source() {
            return Ok(None);
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut response = self.open(
            source.clone(),
            "GET",
            Some("bytes=0-9"),
            Some(deadline),
            &self.cancel,
        )?;
        if !matches!(response.status, 200 | 206) {
            response = self.open(source.clone(), "GET", None, Some(deadline), &self.cancel)?;
        }
        ensure!(
            matches!(response.status, 200 | 206),
            "media probe failed with HTTP {}",
            response.status
        );
        ensure_identity(&response)?;
        let mut prefix = [0; 10];
        let mut used = 0;
        while used < prefix.len() {
            let n = response.body().read(&mut prefix[used..])?;
            if n == 0 {
                break;
            }
            used += n;
        }
        if is_hls(&prefix[..used]) {
            Ok(Some(self.routes.seal(
                &source,
                RouteKind::Playlist,
                "m3u8",
            )?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests;
