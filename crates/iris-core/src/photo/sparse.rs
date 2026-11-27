//! Seekable, byte- and request-limited input for the same parsers used on local files.

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};

use iris_base::net::http_client::{self, HttpCancel};

use super::{Photo, detect_photo_format, finish_header, run_probe};

const PREFIX: usize = 256 << 10;
const READ_AHEAD: usize = 32 << 10;
const MAX_RANGE: usize = 1 << 20;
const MAX_BYTES: usize = 5 << 20;
const MAX_REQUESTS: usize = 16;

type Reply = anyhow::Result<(Vec<u8>, Option<u64>)>;

struct Sparse<'a, F> {
    fetch: F,
    cancel: &'a HttpCancel,
    windows: Vec<(u64, Vec<u8>)>,
    pos: u64,
    end: u64,
    remaining_bytes: usize,
    remaining_requests: usize,
}

impl<'a, F: FnMut(u64, usize) -> Reply> Sparse<'a, F> {
    fn open(mut fetch: F, cancel: &'a HttpCancel) -> Option<Self> {
        if cancel.is_cancelled() {
            return None;
        }
        let (prefix, total) = fetch(0, PREFIX).ok()?;
        let end = total.unwrap_or(prefix.len() as u64);
        if prefix.is_empty() || prefix.len() as u64 > end.min(PREFIX as u64) {
            return None;
        }
        Some(Self {
            fetch,
            cancel,
            pos: 0,
            end,
            remaining_bytes: MAX_BYTES - prefix.len(),
            // An ignored range is a single bounded, self-contained prefix.
            remaining_requests: if total.is_some() { MAX_REQUESTS - 1 } else { 0 },
            windows: vec![(0, prefix)],
        })
    }

    fn window(&mut self, wanted: usize) -> io::Result<&[u8]> {
        let mut next = self.end;
        let mut hit = None;
        for (i, (start, bytes)) in self.windows.iter().enumerate() {
            if let Some(off) = self.pos.checked_sub(*start) {
                if off < bytes.len() as u64 {
                    hit = Some((i, off as usize));
                    break;
                }
            } else {
                next = next.min(*start);
            }
        }
        if let Some((i, off)) = hit {
            return Ok(&self.windows[i].1[off..]);
        }
        let len = wanted
            .clamp(READ_AHEAD, MAX_RANGE)
            .min(self.remaining_bytes) as u64;
        let len = len.min(next - self.pos) as usize;
        if self.remaining_requests == 0 || len == 0 {
            return Err(io::Error::other("photo metadata budget exhausted"));
        }
        self.remaining_requests -= 1;
        match (self.fetch)(self.pos, len) {
            Ok((bytes, Some(total)))
                if total == self.end && !bytes.is_empty() && bytes.len() <= len =>
            {
                self.remaining_bytes -= bytes.len();
                self.windows.push((self.pos, bytes));
                Ok(&self.windows.last().unwrap().1)
            }
            _ => {
                // Stop issuing requests, but leave previously received metadata readable.
                self.remaining_requests = 0;
                Err(io::Error::other("photo metadata range unavailable"))
            }
        }
    }
}

impl<F: FnMut(u64, usize) -> Reply> Read for Sparse<'_, F> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("photo probe cancelled"));
        }
        if out.is_empty() || self.pos >= self.end {
            return Ok(0);
        }
        let bytes = self.window(out.len())?;
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F> Seek for Sparse<'_, F> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.end.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid metadata offset"))?;
        Ok(self.pos)
    }
}

/// Probes remote headers with at most 16 range reads totalling 5 MiB. A server that
/// ignores Range contributes only its initial 256 KiB. Unavailable fields stay unset.
pub fn probe_remote(
    url: &str,
    headers: &BTreeMap<String, String>,
    name_hint: Option<&str>,
    cancel: &HttpCancel,
) -> Photo {
    let Some(mut input) = Sparse::open(
        |start, len| http_client::fetch_range(url, headers, start, len, cancel),
        cancel,
    ) else {
        return Photo::default();
    };
    let mut head = [0; 12];
    let mut len = 0;
    while len < head.len() {
        match input.read(&mut head[len..]) {
            Ok(n) if n > 0 => len += n,
            _ => break,
        }
    }
    input.pos = 0;
    let Some(fmt) = detect_photo_format(&head[..len], name_hint) else {
        return Photo::default();
    };
    finish_header(run_probe(&mut input, fmt), fmt)
}

#[cfg(test)]
#[path = "fixtures.rs"]
mod tests;
