//! Nonblocking cancellable loopback transfer helpers for direct HTTP/WebDAV playback relay.

use std::io::{self, Read};
use std::net::TcpStream;

use anyhow::{Result, ensure};

use crate::loopback_http::{ClientWatch, write_all_cancellable, write_chunk_cancellable};
use crate::vfs::HttpCancel;

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
    let Some((start, end)) = value.strip_prefix("bytes=").and_then(|s| s.split_once('-')) else {
        return false;
    };
    let integer = |s: &str| s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok();
    (!start.is_empty() || !end.is_empty())
        && (start.is_empty() || integer(start))
        && (end.is_empty() || integer(end))
}
