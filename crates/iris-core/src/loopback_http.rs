//! Shared loopback HTTP/1.1 nonblocking I/O primitives used by both direct
//! HTTP playback relays and plugin-scoped stream relays.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mio::{Interest, Registry, Token};

use crate::vfs::HttpCancel;

struct ClientState {
    cancel: HttpCancel,
    writable: Arc<(Mutex<u64>, Condvar)>,
}

/// Multiplexes client disconnect cancellation and nonblocking write readiness
/// onto a relay listener's `mio::Poll` loop.
pub(crate) struct ClientHub {
    registry: Registry,
    next_token: AtomicUsize,
    clients: Mutex<BTreeMap<usize, ClientState>>,
}

impl ClientHub {
    pub(crate) fn new(registry: Registry, first_token: usize) -> Self {
        Self {
            registry,
            next_token: AtomicUsize::new(first_token),
            clients: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn watch(
        self: &Arc<Self>,
        socket: &TcpStream,
        cancel: HttpCancel,
    ) -> io::Result<ClientWatch> {
        let token = self
            .next_token
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("relay client token exhausted"))?;
        let cloned = socket.try_clone()?;
        let mut stream = mio::net::TcpStream::from_std(cloned);
        let writable = Arc::new((Mutex::new(0u64), Condvar::new()));
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token,
                ClientState {
                    cancel,
                    writable: writable.clone(),
                },
            );
        if let Err(err) = self
            .registry
            .register(&mut stream, Token(token), Interest::READABLE)
        {
            self.clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&token);
            return Err(err);
        }
        Ok(ClientWatch {
            hub: Arc::clone(self),
            token,
            stream,
            writable,
            armed_writable: false,
        })
    }

    /// Processes a client `mio` event (any token >= `first_token`).
    pub(crate) fn dispatch_event(&self, event: &mio::event::Event) {
        let Token(id) = event.token();
        if event.is_readable()
            || event.is_read_closed()
            || event.is_write_closed()
            || event.is_error()
        {
            let client = self
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
        } else if event.is_writable() {
            let writable = self
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
    }
}

pub(crate) struct ClientWatch {
    hub: Arc<ClientHub>,
    token: usize,
    stream: mio::net::TcpStream,
    writable: Arc<(Mutex<u64>, Condvar)>,
    armed_writable: bool,
}

impl ClientWatch {
    pub(crate) fn writable_gen(&self) -> u64 {
        *self.writable.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn wait_writable(&mut self, seen_gen: u64, cancel: &HttpCancel) -> io::Result<()> {
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        if let Err(err) = self.hub.registry.reregister(
            &mut self.stream,
            Token(self.token),
            Interest::READABLE | Interest::WRITABLE,
        ) {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "relay cancelled",
                ));
            }
            return Err(err);
        }
        self.armed_writable = true;
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        let guard = self.writable.0.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == seen_gen {
            let _ = self
                .writable
                .1
                .wait_timeout(guard, Duration::from_millis(100));
        }
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        Ok(())
    }

    pub(crate) fn disarm_writable(&mut self) {
        if self.armed_writable {
            self.armed_writable = false;
            let _ = self.hub.registry.reregister(
                &mut self.stream,
                Token(self.token),
                Interest::READABLE,
            );
        }
    }
}

impl Drop for ClientWatch {
    fn drop(&mut self) {
        self.hub
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
        let _ = self.hub.registry.deregister(&mut self.stream);
    }
}

/// Reads an HTTP/1.1 request header block terminated by `\r\n\r\n` within `timeout` and `max_bytes`.
pub(crate) fn read_request(
    socket: &mut TcpStream,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    socket.set_nonblocking(false)?;
    let deadline = Instant::now() + timeout;
    let mut raw = Vec::with_capacity(2048);
    let mut buf = [0u8; 2048];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "relay request timeout"))?;
        socket.set_read_timeout(Some(left))?;
        let n = socket.read(&mut buf)?;
        if n == 0 || raw.len() + n > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid relay request size",
            ));
        }
        let scan = raw.len().saturating_sub(3);
        raw.extend_from_slice(&buf[..n]);
        if raw[scan..].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(raw);
        }
    }
}

/// Writes `bytes` to a nonblocking loopback socket, waiting for `mio` writability
/// and aborting promptly if `cancel` fires.
pub(crate) fn write_all_cancellable(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if cancel.is_cancelled() {
            watch.disarm_writable();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay cancelled",
            ));
        }
        let epoch = watch.writable_gen();
        match socket.write(bytes) {
            Ok(0) => {
                watch.disarm_writable();
                return Err(io::Error::new(io::ErrorKind::WriteZero, "relay write zero"));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Err(wait_err) = watch.wait_writable(epoch, cancel) {
                    watch.disarm_writable();
                    return Err(wait_err);
                }
            }
            Err(err) => {
                watch.disarm_writable();
                return Err(err);
            }
        }
    }
    watch.disarm_writable();
    Ok(())
}

/// Writes one payload slice, optionally wrapped in HTTP/1.1 chunked framing.
pub(crate) fn write_chunk_cancellable(
    socket: &mut TcpStream,
    bytes: &[u8],
    chunked: bool,
    watch: &mut ClientWatch,
    cancel: &HttpCancel,
) -> io::Result<()> {
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

/// Returns `true` when `bytes` begins like an HLS (`#EXTM3U`) or XML/DASH (`<...`) manifest.
pub(crate) fn is_adaptive_manifest(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let trimmed = bytes.trim_ascii_start();
    trimmed.starts_with(b"#EXTM3U")
        || trimmed == b"<"
        || (trimmed.len() >= 2
            && trimmed[0] == b'<'
            && matches!(trimmed[1], b'?' | b'!' | b'a'..=b'z' | b'A'..=b'Z' | b'/'))
}

/// Returns `true` while `bytes` is still too short to decide whether it begins with
/// an adaptive manifest header.
pub(crate) fn is_adaptive_prefix_undecided(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes == b"\xef" || bytes == b"\xef\xbb" {
        return true;
    }
    let trimmed = bytes
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(bytes)
        .trim_ascii_start();
    trimmed.is_empty()
        || (trimmed.len() < 7 && b"#EXTM3U".starts_with(trimmed))
        || (trimmed.len() < 2 && trimmed[0] == b'<')
}
