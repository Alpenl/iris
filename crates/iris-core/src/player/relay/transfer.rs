//! Nonblocking cancellable loopback transfer helpers for direct HTTP/WebDAV playback relay.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use super::{ClientWatch, MAX_REQUEST};
use crate::vfs::HttpCancel;

pub(super) fn read_request(socket: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
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

pub(super) fn ensure_identity(response: &crate::vfs::http_client::HttpStream) -> Result<()> {
    ensure!(
        response
            .headers
            .get("content-encoding")
            .is_none_or(|v| v.eq_ignore_ascii_case("identity")),
        "unsupported media content encoding"
    );
    Ok(())
}

pub(super) fn write_nonblocking(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    watch: &mut ClientWatch<'_>,
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

pub(super) fn copy_stream(
    mut body: impl Read,
    socket: &mut TcpStream,
    chunked: bool,
    watch: &mut ClientWatch<'_>,
    cancel: &HttpCancel,
) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = match body.read(&mut buffer) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            if chunked {
                write_nonblocking(socket, b"0\r\n\r\n", watch, cancel)?;
            }
            return Ok(());
        }
        if chunked {
            let header = format!("{n:x}\r\n");
            write_nonblocking(socket, header.as_bytes(), watch, cancel)?;
        }
        write_nonblocking(socket, &buffer[..n], watch, cancel)?;
        if chunked {
            write_nonblocking(socket, b"\r\n", watch, cancel)?;
        }
    }
}

pub(super) fn valid_range(value: &str) -> bool {
    let Some((start, end)) = value.strip_prefix("bytes=").and_then(|s| s.split_once('-')) else {
        return false;
    };
    let integer = |s: &str| s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok();
    (!start.is_empty() || !end.is_empty())
        && (start.is_empty() || integer(start))
        && (end.is_empty() || integer(end))
}
