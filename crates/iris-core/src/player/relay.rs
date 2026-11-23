//! Authenticated playback transport. FFmpeg's HTTP `headers` option is also used
//! for redirects and inherited by HLS child contexts. Keep credentials in Iris and
//! give libmpv encrypted, playback-scoped resource URLs instead.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mio::{Events, Interest, Poll, Token, Waker};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use url::Url;

use crate::vfs::http_client::{
    HttpCancel, NetworkScope, Request, open_stream, validate_stream_url,
};

use super::model::fetch_subtitle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_URL: usize = 2800;
const MAX_REQUEST: usize = 24 * 1024;
const MAX_PLAYLIST: usize = 4 * 1024 * 1024;
const MAX_REWRITTEN_PLAYLIST: usize = 16 * 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

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
                        "m3u8", "m3u", "mp4", "m4v", "m4a", "m4s", "cmfv", "cmfa", "ts", "m2ts",
                        "aac", "ac3", "ec3", "mp3", "vtt", "webvtt", "srt", "ass", "ssa", "webm",
                        "mkv", "mov", "ogg", "flac", "wav", "jpg", "jpeg", "png", "heic", "avif",
                        "webp", "key",
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
}

pub(super) struct PlaybackRelay {
    service: Arc<Service>,
    listener: Option<thread::JoinHandle<()>>,
    wake: Waker,
}

impl PlaybackRelay {
    pub(super) fn start(url: Url, headers: BTreeMap<String, String>) -> Result<Self> {
        const LISTENER: Token = Token(0);
        const CANCEL: Token = Token(1);
        let mut listener =
            mio::net::TcpListener::bind(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)))?;
        let mut poll = Poll::new()?;
        let wake = Waker::new(poll.registry(), CANCEL)?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        let service = Arc::new(Service {
            routes: Routes::new(listener.local_addr()?)?,
            scope: NetworkScope::new(url),
            headers,
            cancel: HttpCancel::default(),
            workers: AtomicUsize::new(0),
        });
        let server = Arc::clone(&service);
        let listener = thread::Builder::new()
            .name("iris-media-relay".into())
            .spawn(move || {
                let mut events = Events::with_capacity(2);
                let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
                while !server.cancel.is_cancelled() {
                    match poll.poll(&mut events, None) {
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                        Ok(()) => (),
                    }
                    if !events.iter().any(|event| event.token() == LISTENER) {
                        continue;
                    }
                    while !server.cancel.is_cancelled() {
                        let socket = match listener.accept() {
                            Ok((socket, _)) => TcpStream::from(socket),
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        };
                        workers.retain(|worker| !worker.is_finished());
                        if let Some(worker) = server.dispatch(socket) {
                            workers.push(worker);
                        }
                    }
                }
                server.cancel.cancel();
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Self {
            service,
            listener: Some(listener),
            wake,
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
        let _ = self.wake.wake();
    }
}

impl Drop for PlaybackRelay {
    fn drop(&mut self) {
        self.cancel();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

struct Worker(Arc<Service>);

impl Drop for Worker {
    fn drop(&mut self) {
        self.0.workers.fetch_sub(1, Ordering::Relaxed);
        ACTIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Service {
    fn dispatch(self: &Arc<Self>, mut socket: TcpStream) -> Option<thread::JoinHandle<()>> {
        if self.workers.load(Ordering::Relaxed) >= 8
            || ACTIVE_WORKERS
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < 32).then_some(n + 1)
                })
                .is_err()
        {
            // Drain the ordinary small request before replying so closing the
            // socket does not discard the response as an unread-data reset.
            let _ = read_request(&mut socket, Duration::from_millis(100));
            let _ = socket.set_write_timeout(Some(Duration::from_millis(100)));
            let _ = socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            return None;
        }
        self.workers.fetch_add(1, Ordering::Relaxed);
        let worker = Worker(Arc::clone(self));
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
        let raw = read_request(socket, CLIENT_TIMEOUT)?;
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
        if kind == RouteKind::Subtitle {
            // Remote attachments share the media's address policy even when
            // their origin has no access to the media request headers.
            let empty = BTreeMap::new();
            let source = self.scope.source();
            let (headers, credentials) = if source.origin() == url.origin() {
                (&self.headers, source)
            } else {
                (&empty, &url)
            };
            let text = fetch_subtitle(&url, headers, credentials, &self.scope, &self.cancel)?;
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
        let mut response = self.open(url, method, if playlist { None } else { range }, deadline)?;
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

        let chunked = method == "GET" && !response.headers.contains_key("content-length");
        *committed = true;
        write!(
            socket,
            "HTTP/1.1 {} OK\r\nConnection: close\r\nCache-Control: no-store\r\n",
            response.status
        )?;
        for name in [
            "content-type",
            "content-length",
            "content-range",
            "accept-ranges",
        ] {
            if name != "content-type" && playlist {
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
        if method == "GET" {
            if chunked {
                copy_chunked(response.body(), socket)?;
            } else {
                io::copy(response.body(), socket)?;
            }
        }
        Ok(())
    }

    fn open(
        &self,
        url: Url,
        method: &str,
        range: Option<&str>,
        deadline: Option<Instant>,
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
            &self.cancel,
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
        let mut response = self.open(source.clone(), "GET", Some("bytes=0-9"), Some(deadline))?;
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

fn read_request(socket: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
    socket.set_nonblocking(false)?;
    let deadline = Instant::now() + timeout;
    let mut raw = Vec::with_capacity(2048);
    let mut buf = [0; 2048];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("relay request timeout")?;
        socket.set_read_timeout(Some(left))?;
        let n = socket.read(&mut buf)?;
        ensure!(
            n != 0 && raw.len() + n <= MAX_REQUEST,
            "invalid relay request size"
        );
        let scan = raw.len().saturating_sub(3);
        raw.extend_from_slice(&buf[..n]);
        if raw[scan..].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(raw);
        }
    }
}

fn ensure_identity(response: &crate::vfs::http_client::HttpStream) -> Result<()> {
    ensure!(
        response
            .headers
            .get("content-encoding")
            .is_none_or(|v| v.eq_ignore_ascii_case("identity")),
        "unsupported media content encoding"
    );
    Ok(())
}

fn is_hls(prefix: &[u8]) -> bool {
    prefix
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(prefix)
        .starts_with(b"#EXTM3U")
}

fn copy_chunked(mut body: impl Read, socket: &mut TcpStream) -> io::Result<()> {
    let mut buffer = [0; 16 * 1024];
    loop {
        let n = match body.read(&mut buffer) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return socket.write_all(b"0\r\n\r\n");
        }
        write!(socket, "{n:x}\r\n")?;
        socket.write_all(&buffer[..n])?;
        socket.write_all(b"\r\n")?;
    }
}

fn valid_range(value: &str) -> bool {
    let Some((start, end)) = value.strip_prefix("bytes=").and_then(|s| s.split_once('-')) else {
        return false;
    };
    let integer = |s: &str| s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok();
    (!start.is_empty() || !end.is_empty())
        && (start.is_empty() || integer(start))
        && (end.is_empty() || integer(end))
}

fn rewrite_playlist(input: &str, base: &Url, routes: &Routes) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let lines = input.trim_start_matches('\u{feff}');
    ensure!(
        !lines.contains('\0') && !lines.contains("{$"),
        "unsupported HLS playlist syntax"
    );
    ensure!(
        lines.lines().next() == Some("#EXTM3U"),
        "invalid HLS playlist"
    );
    let mut variant = false;
    let mut rewritten = BTreeMap::new();
    for line in lines.lines() {
        // Match the demuxer's treatment of surrounding whitespace and reject
        // control characters that could create a second logical line.
        ensure!(
            line.bytes().all(|b| b >= 0x20 || b == b'\t'),
            "invalid HLS line"
        );
        let line = line.trim_ascii();
        let before = out.len();
        if line.starts_with('#') {
            ensure!(
                !line.starts_with("#EXT-X-DEFINE:")
                    && !line.starts_with("#EXT-X-CONTENT-STEERING:"),
                "unsupported HLS playlist indirection"
            );
            if line.starts_with("#EXT-X-STREAM-INF:") {
                variant = true;
            }
            if line.starts_with("#EXT-X-")
                && let Some((tag, attrs)) = line.split_once(':')
            {
                out.push_str(tag);
                out.push(':');
                rewrite_attributes(tag, attrs, base, routes, &mut rewritten, &mut out)?;
            } else {
                out.push_str(line);
            }
        } else if !line.is_empty() {
            let url = validate_stream_url(base.join(line)?.as_str())?;
            let (kind, ext) = if variant {
                (RouteKind::Playlist, "m3u8")
            } else {
                (RouteKind::Media, "ts")
            };
            rewrite_uri(url, kind, ext, routes, &mut rewritten, &mut out)?;
            variant = false;
        }
        ensure!(
            out.len() - before < 4096,
            "HLS line exceeds the demuxer limit"
        );
        out.push('\n');
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
    }
    Ok(out)
}

type RewrittenUris = BTreeMap<(Url, &'static str, &'static str), String>;

fn rewrite_uri(
    url: Url,
    kind: RouteKind,
    extension: &'static str,
    routes: &Routes,
    rewritten: &mut RewrittenUris,
    out: &mut String,
) -> Result<()> {
    // Byte ranges and keys can reference the same representation more than once.
    use std::collections::btree_map::Entry;
    let local = match rewritten.entry((url, kind.tag(), extension)) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let local = routes.seal(&entry.key().0, kind, extension)?;
            entry.insert(local)
        }
    };
    out.push_str(local);
    Ok(())
}

/// Rewrite complete URI attributes, respecting commas inside quoted values (RFC 8216 §4.2).
fn rewrite_attributes(
    tag: &str,
    mut attrs: &str,
    base: &Url,
    routes: &Routes,
    rewritten: &mut RewrittenUris,
    out: &mut String,
) -> Result<()> {
    while !attrs.is_empty() {
        let mut quoted = false;
        let end = attrs
            .bytes()
            .position(|b| {
                if b == b'"' {
                    quoted = !quoted;
                }
                b == b',' && !quoted
            })
            .unwrap_or(attrs.len());
        ensure!(!quoted, "unterminated HLS attribute");
        let attr = &attrs[..end];
        if let Some((key, value)) = attr.split_once('=')
            && (key.trim() == "URI" || key.trim().ends_with("-URI"))
        {
            let raw = value
                .trim()
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .context("invalid HLS URI attribute")?;
            let url = validate_stream_url(base.join(raw)?.as_str())?;
            out.push_str(key);
            out.push_str("=\"");
            let (kind, fallback) = match tag {
                "#EXT-X-MEDIA"
                | "#EXT-X-I-FRAME-STREAM-INF"
                | "#EXT-X-IMAGE-STREAM-INF"
                | "#EXT-X-RENDITION-REPORT" => (RouteKind::Playlist, "m3u8"),
                "#EXT-X-KEY" | "#EXT-X-SESSION-KEY" => (RouteKind::Media, "key"),
                "#EXT-X-MAP" => (RouteKind::Media, "mp4"),
                _ => (RouteKind::Media, "ts"),
            };
            rewrite_uri(url, kind, fallback, routes, rewritten, out)?;
            out.push('"');
        } else {
            out.push_str(attr);
        }
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
        attrs = &attrs[end..];
        if let Some(rest) = attrs.strip_prefix(',') {
            out.push(',');
            attrs = rest;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
