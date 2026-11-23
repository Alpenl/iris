//! Authenticated playback transport. FFmpeg's HTTP `headers` option is also used
//! for redirects and inherited by HLS child contexts. Keep credentials in Iris and
//! give libmpv encrypted, playback-scoped resource URLs instead.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use url::Url;

use crate::vfs::http_client::{
    HttpCancel, NetworkScope, Request, open_stream, validate_stream_url,
};

const MAX_URL: usize = 8192;
const MAX_REQUEST: usize = 24 * 1024;
const MAX_PLAYLIST: usize = 4 * 1024 * 1024;
const MAX_REWRITTEN_PLAYLIST: usize = 16 * 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

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

    fn seal(&self, url: &Url, fallback_ext: &str) -> Result<String> {
        ensure!(url.as_str().len() <= MAX_URL, "media URL exceeds limit");
        // FFmpeg checks the suffix of HLS segments. Expose only known format hints.
        let ext = url
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .and_then(|name| name.rsplit_once('.'))
            .and_then(|(_, suffix)| {
                [
                    "m3u8", "m3u", "mp4", "m4v", "m4a", "m4s", "cmfv", "cmfa", "ts", "m2ts", "aac",
                    "ac3", "ec3", "mp3", "vtt", "webvtt", "srt", "ass", "ssa", "webm", "mkv",
                    "mov", "ogg", "flac", "wav", "jpg", "jpeg", "png", "heic", "avif", "webp",
                    "key",
                ]
                .into_iter()
                .find(|ext| ext.eq_ignore_ascii_case(suffix))
            })
            .unwrap_or(fallback_ext);
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
                Aad::from(ext.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| anyhow!("cannot authorize playback resource"))?;
        Ok(format!(
            "http://{}/{sequence:x}/{}.{ext}",
            self.authority,
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    fn open(&self, path: &str) -> Result<Url> {
        let (sequence, payload) = path
            .strip_prefix('/')
            .and_then(|p| p.split_once('/'))
            .context("invalid playback route")?;
        let (payload, ext) = payload.rsplit_once('.').context("invalid playback route")?;
        let mut iv = [0; 12];
        iv[4..].copy_from_slice(&u64::from_str_radix(sequence, 16)?.to_be_bytes());
        let mut bytes = URL_SAFE_NO_PAD.decode(payload)?;
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(iv),
                Aad::from(ext.as_bytes()),
                &mut bytes,
            )
            .map_err(|_| anyhow!("invalid playback capability"))?;
        validate_stream_url(std::str::from_utf8(plain)?)
    }
}

struct Service {
    routes: Routes,
    scope: NetworkScope,
    headers: BTreeMap<String, String>,
    cancel: HttpCancel,
    workers: AtomicUsize,
}

pub(super) struct PlaybackRelay {
    service: Arc<Service>,
    listener: thread::JoinHandle<()>,
}

impl PlaybackRelay {
    pub(super) fn start(url: Url, headers: BTreeMap<String, String>) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
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
                while !server.cancel.is_cancelled() {
                    match listener.accept() {
                        Ok((socket, _)) => server.dispatch(socket),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(15))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self { service, listener })
    }

    pub(super) fn url(&self, url: &Url) -> Result<String> {
        self.service.routes.seal(url, "media")
    }

    pub(super) fn cancel(&self) {
        self.service.cancel.cancel();
        self.listener.thread().unpark();
    }
}

impl Drop for PlaybackRelay {
    fn drop(&mut self) {
        self.cancel();
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
    fn dispatch(self: &Arc<Self>, mut socket: TcpStream) {
        if self.workers.load(Ordering::Relaxed) >= 8
            || ACTIVE_WORKERS
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < 32).then_some(n + 1)
                })
                .is_err()
        {
            return;
        }
        self.workers.fetch_add(1, Ordering::Relaxed);
        let worker = Worker(Arc::clone(self));
        let _ = thread::Builder::new().name("iris-media-request".into()).spawn(move || {
            let service = &worker.0;
            let Ok(_registration) = service.cancel.register(&socket) else { return };
            let mut committed = false;
            if service.serve(&mut socket, &mut committed).is_err() && !committed {
                let _ = socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            }
        });
    }

    fn serve(&self, socket: &mut TcpStream, committed: &mut bool) -> Result<()> {
        socket.set_write_timeout(Some(CLIENT_TIMEOUT))?;
        let raw = read_request(socket)?;
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
        let url = self
            .routes
            .open(request.path.context("missing playback route")?)?;
        let known_playlist = url.path().to_ascii_lowercase().ends_with(".m3u8");
        let upstream = |method, range| {
            open_stream(
                Request {
                    method,
                    url: url.clone(),
                    headers: &self.headers,
                    body: None,
                    range,
                },
                &self.scope,
                &self.cancel,
                None,
            )
        };
        let mut response = upstream(method, if known_playlist { None } else { range })?;
        if !matches!(response.status, 200 | 206) {
            *committed = true;
            write!(
                socket,
                "HTTP/1.1 {} Upstream response\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                response.status
            )?;
            return Ok(());
        }
        ensure!(
            response
                .headers
                .get("content-encoding")
                .is_none_or(|v| v.eq_ignore_ascii_case("identity")),
            "unsupported media content encoding"
        );
        let mut prefix = [0; 7];
        let mut used = 0;
        if method == "GET" {
            while used < prefix.len() {
                let n = response.body.read(&mut prefix[used..])?;
                if n == 0 {
                    break;
                }
                used += n;
            }
        }
        let hls = known_playlist
            || response.url.path().to_ascii_lowercase().ends_with(".m3u8")
            || prefix[..used] == *b"#EXTM3U"
            || response
                .headers
                .get("content-type")
                .is_some_and(|s| s.to_ascii_lowercase().contains("mpegurl"));
        if hls && method == "GET" {
            if response.status == 206 {
                response = upstream("GET", None)?;
                ensure!(response.status == 200, "incomplete HLS playlist");
                used = 0;
            }
            let mut data = Vec::from(&prefix[..used]);
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut buf = [0; 8192];
            loop {
                ensure!(Instant::now() < deadline, "playlist fetch timed out");
                let cap = (MAX_PLAYLIST + 1 - data.len()).min(buf.len());
                let n = response.body.read(&mut buf[..cap])?;
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                ensure!(
                    data.len() <= MAX_PLAYLIST,
                    "HLS playlist exceeds byte limit"
                );
            }
            let text = rewrite_playlist(std::str::from_utf8(&data)?, &response.url, &self.routes)?;
            *committed = true;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
                text.len()
            )?;
            socket.write_all(text.as_bytes())?;
        } else {
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
                if hls && name != "content-type" {
                    continue;
                }
                if let Some(value) = response.headers.get(name) {
                    write!(socket, "{name}: {value}\r\n")?;
                }
            }
            socket.write_all(b"\r\n")?;
            if method == "GET" {
                socket.write_all(&prefix[..used])?;
                io::copy(&mut response.body, socket)?;
            }
        }
        Ok(())
    }
}

fn read_request(socket: &mut TcpStream) -> Result<Vec<u8>> {
    // Windows can inherit the listening socket's nonblocking mode on accept.
    socket.set_nonblocking(false)?;
    let deadline = Instant::now() + CLIENT_TIMEOUT;
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
        lines.lines().next() == Some("#EXTM3U"),
        "invalid HLS playlist"
    );
    let mut variant = false;
    for line in lines.lines() {
        if line.starts_with('#') {
            if line.starts_with("#EXT-X-STREAM-INF:") {
                variant = true;
            }
            if line.starts_with("#EXT-X-")
                && let Some((tag, attrs)) = line.split_once(':')
            {
                out.push_str(tag);
                out.push(':');
                rewrite_attributes(attrs, base, routes, &mut out)?;
            } else {
                out.push_str(line);
            }
        } else if !line.trim().is_empty() {
            let url = validate_stream_url(base.join(line.trim())?.as_str())?;
            out.push_str(&routes.seal(&url, if variant { "m3u8" } else { "ts" })?);
            variant = false;
        }
        out.push('\n');
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
    }
    Ok(out)
}

/// Rewrite complete URI attributes, respecting commas inside quoted values (RFC 8216 §4.2).
fn rewrite_attributes(
    mut attrs: &str,
    base: &Url,
    routes: &Routes,
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
        let attr = &attrs[..end];
        if let Some((key, value)) = attr.split_once('=')
            && matches!(key.trim(), "URI" | "SERVER-URI")
        {
            let raw = value
                .trim()
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .context("invalid HLS URI attribute")?;
            let url = validate_stream_url(base.join(raw)?.as_str())?;
            out.push_str(key);
            out.push_str("=\"");
            out.push_str(&routes.seal(&url, "media")?);
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
