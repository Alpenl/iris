//! Nonblocking cancellable loopback transfer helpers for direct HTTP/WebDAV playback relay.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Result, ensure};

use iris_base::net::http_client::{HttpCancel, HttpStream};
use iris_base::net::loopback_http::{
    ClientWatch, HttpRequestError, read_request, write_all_cancellable, write_chunk_cancellable,
};

pub(super) enum RelayError {
    Request(HttpRequestError),
    Timeout,
    BadGateway,
}

impl RelayError {
    pub(super) const fn status(self) -> (u16, &'static str) {
        match self {
            Self::Request(err) => (err.status_code(), err.reason_phrase()),
            Self::Timeout => (408, "Request Timeout"),
            Self::BadGateway => (502, "Bad Gateway"),
        }
    }
}

pub(super) fn write_error_response(socket: &mut TcpStream, err: RelayError) {
    let (status, reason) = err.status();
    let extra = if status == 416 {
        "Content-Range: bytes */*\r\n"
    } else {
        ""
    };
    let _ = write!(
        socket,
        "HTTP/1.1 {status} {reason}\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n"
    );
}

pub(super) fn reject_busy(mut socket: TcpStream, max_request: usize) {
    // Drain the ordinary small request before replying so closing the
    // socket does not discard the response as an unread-data reset.
    let _ = read_request(&mut socket, Duration::from_millis(100), max_request);
    let _ = socket.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = socket.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    );
}

pub(super) fn ensure_identity(response: &HttpStream) -> Result<()> {
    ensure!(
        response
            .headers
            .get("content-encoding")
            .is_none_or(|v| v.eq_ignore_ascii_case("identity")),
        "unsupported media content encoding"
    );
    Ok(())
}

pub(super) fn copy_stream(
    mut body: impl Read,
    socket: &mut TcpStream,
    chunked: bool,
    watch: &mut ClientWatch,
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
                write_all_cancellable(socket, b"0\r\n\r\n", watch, cancel)?;
            }
            return Ok(());
        }
        write_chunk_cancellable(socket, &buffer[..n], chunked, watch, cancel)?;
    }
}

pub(super) fn valid_range(value: &str) -> bool {
    iris_base::net::range::ByteRangeSpec::parse(value).is_ok()
}
