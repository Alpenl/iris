//! Continuous single-connection upstream streaming for unverified direct HTTP
//! playback, with verified exact-offset upstream resume if a CDN closes an idle
//! connection while the player pauses or plays from demuxer cache.
use super::{
    Metadata, RemoteReader, RemoteTarget, Response, content_range, identity_encoding, unique,
};
use crate::{
    network::raw::RawState,
    stream::reader::{OpenedStream, StreamSlice},
};
use iris_api::v2::ErrorCode;
use iris_base::net::http_client::HttpCancel;
use iris_core::logging;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const STREAM_CHUNK: usize = 64 << 10;
const STREAM_RESERVATION: u64 = 256 << 10;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_RESUMES: u8 = 3;

struct EmptySlice;
impl StreamSlice for EmptySlice {
    fn read_chunk(&mut self, _out: &mut [u8]) -> Result<usize, ErrorCode> {
        Ok(0)
    }
}

struct RemoteStream {
    target: Arc<RemoteTarget>,
    response: Option<Response>,
    offset: u64,
    remaining: Option<u64>,
    total: Option<u64>,
    exact_length: bool,
    consecutive_resumes: u8,
    cancel: HttpCancel,
}

impl RemoteStream {
    fn can_resume(&self) -> bool {
        self.exact_length
            && self.remaining.is_some_and(|left| left > 0)
            && self.consecutive_resumes < MAX_CONSECUTIVE_RESUMES
            && !self.cancel.is_cancelled()
            && self.target.grant.check_live().is_ok()
    }

    fn resume_upstream(&mut self, reason: &'static str) -> Result<(), ErrorCode> {
        let Some(left) = self.remaining.filter(|&n| n > 0) else {
            return Err(ErrorCode::IoError);
        };
        let end = self
            .offset
            .checked_add(left - 1)
            .ok_or(ErrorCode::RangeInvalid)?;
        // Drop the dead upstream response first so its Resource::Http reservation
        // is returned to the ledger before opening the replacement connection.
        self.response = None;
        self.consecutive_resumes = self.consecutive_resumes.saturating_add(1);
        let attempt = self.consecutive_resumes;
        let known_total = self.total.or(self.target.expected);
        let host = logging::url_host(&self.target.resource.url);
        let (resp, retried) = self.target.open_range_with_fallback(
            Some((self.offset, Some(end))),
            known_total,
            true,
            true,
            STREAM_RESERVATION,
            &self.cancel,
            || Instant::now() + STREAM_IDLE_TIMEOUT,
        )?;
        let RawState::Response(raw) = &resp.state else {
            return Err(ErrorCode::Internal);
        };
        identity_encoding(&raw.headers)?;
        if raw.status != 206 {
            let code = if matches!(raw.status, 401 | 403) {
                ErrorCode::AuthFailed
            } else {
                ErrorCode::IoError
            };
            logging::event(
                logging::EventLevel::Warn,
                "stream",
                "upstream_resume_failed",
                json!({
                    "host": host,
                    "offset": self.offset,
                    "remaining": left,
                    "status": raw.status,
                    "attempt": attempt,
                    "reason": reason,
                    "error": format!("{code:?}"),
                }),
            );
            return Err(code);
        }
        let (start, count, new_total) =
            content_range(unique(&raw.headers, "content-range")?.ok_or(ErrorCode::RangeInvalid)?)?;
        if known_total.is_some() && new_total.is_some() && new_total != known_total {
            return Err(ErrorCode::StaleHandle);
        }
        if start != self.offset
            || count != left
            || unique(&raw.headers, "content-length")?
                .map(super::super::range::decimal)
                .transpose()?
                .is_some_and(|n| n != count)
        {
            return Err(ErrorCode::RangeInvalid);
        }
        if self.offset > 0 && !retried {
            self.target.remember(&raw.headers, new_total, true)?;
        } else {
            self.target.remember_stream(&raw.headers, new_total, true)?;
        }
        logging::event(
            logging::EventLevel::Info,
            "stream",
            "upstream_resumed",
            json!({
                "host": host,
                "offset": self.offset,
                "remaining": left,
                "attempt": attempt,
                "reason": reason,
            }),
        );
        self.response = Some(resp);
        Ok(())
    }
}

impl StreamSlice for RemoteStream {
    fn read_chunk(&mut self, out: &mut [u8]) -> Result<usize, ErrorCode> {
        loop {
            self.target.grant.check_live()?;
            if self.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if self.remaining == Some(0) || out.is_empty() {
                return Ok(0);
            }
            let cap = self
                .remaining
                .map_or(out.len(), |left| out.len().min(left as usize));
            let deadline = Instant::now() + STREAM_IDLE_TIMEOUT;
            let read_res = match self.response.as_mut() {
                Some(resp) => resp.state.read(&mut out[..cap], deadline),
                None => Err(ErrorCode::IoError),
            };
            self.target.grant.check_live()?;
            if self.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            match read_res {
                Ok(0) => {
                    if self.exact_length && self.remaining.is_some_and(|left| left > 0) {
                        if self.can_resume() {
                            self.resume_upstream("premature_eof")?;
                            continue;
                        }
                        return Err(ErrorCode::IoError);
                    }
                    self.remaining = Some(0);
                    return Ok(0);
                }
                Ok(n) => {
                    self.consecutive_resumes = 0;
                    self.offset = self.offset.saturating_add(n as u64);
                    if let Some(left) = &mut self.remaining {
                        *left = left.saturating_sub(n as u64);
                    }
                    return Ok(n);
                }
                Err(err @ (ErrorCode::IoError | ErrorCode::Timeout)) if self.can_resume() => {
                    let reason = if err == ErrorCode::Timeout {
                        "timeout"
                    } else {
                        "io_error"
                    };
                    self.resume_upstream(reason)?;
                }
                Err(err) => {
                    logging::event(
                        logging::EventLevel::Warn,
                        "stream",
                        "upstream_read_failed",
                        json!({
                            "host": logging::url_host(&self.target.resource.url),
                            "offset": self.offset,
                            "remaining": self.remaining,
                            "error": format!("{err:?}"),
                        }),
                    );
                    return Err(err);
                }
            }
        }
    }
}

impl RemoteReader {
    pub(super) fn open_continuous(
        &self,
        offset: u64,
        len: Option<u64>,
        cancel: &HttpCancel,
    ) -> Result<OpenedStream, ErrorCode> {
        self.target.grant.check_live()?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let known = self.target.known_size();
        if known.is_some_and(|size| offset > size) {
            return Err(ErrorCode::RangeInvalid);
        }
        if known == Some(offset) || len == Some(0) {
            return Ok(OpenedStream {
                first: Vec::new(),
                eof: true,
                total: known,
                remaining: Some(0),
                rest: Box::new(EmptySlice),
            });
        }
        let target_len = match (known, len) {
            (Some(size), Some(req)) => Some(req.min(size - offset)),
            (Some(size), None) => Some(size - offset),
            (None, req) => req,
        };
        let range = match target_len {
            Some(0) => {
                return Ok(OpenedStream {
                    first: Vec::new(),
                    eof: true,
                    total: known,
                    remaining: Some(0),
                    rest: Box::new(EmptySlice),
                });
            }
            Some(l) => {
                let end = offset.checked_add(l - 1).ok_or(ErrorCode::RangeInvalid)?;
                Some((offset, Some(end)))
            }
            None if offset > 0 => Some((offset, None)),
            None => None,
        };

        let (mut response, retried) = self.target.open_range_with_fallback(
            range,
            known,
            offset > 0,
            offset > 0,
            STREAM_RESERVATION,
            cancel,
            || Instant::now() + STREAM_IDLE_TIMEOUT,
        )?;

        let RawState::Response(raw) = &response.state else {
            return Err(ErrorCode::Internal);
        };
        identity_encoding(&raw.headers)?;
        if raw.status == 416 {
            let total = unique(&raw.headers, "content-range")?
                .and_then(|v| v.strip_prefix("bytes */"))
                .ok_or(ErrorCode::RangeInvalid)?;
            let total = super::super::range::decimal(total)?;
            if offset > 0 && !retried {
                self.target.remember(&raw.headers, Some(total), false)?;
            } else {
                self.target
                    .remember_stream(&raw.headers, Some(total), offset == 0 || retried)?;
            }
            if offset != total {
                return Err(ErrorCode::RangeInvalid);
            }
            return Ok(OpenedStream {
                first: Vec::new(),
                eof: true,
                total: Some(total),
                remaining: Some(0),
                rest: Box::new(EmptySlice),
            });
        }
        let (expected, total, exact_length) = if raw.status == 206 {
            let (start, count, total) = content_range(
                unique(&raw.headers, "content-range")?.ok_or(ErrorCode::RangeInvalid)?,
            )?;
            if start != offset
                || target_len.is_some_and(|l| count > l)
                || unique(&raw.headers, "content-length")?
                    .map(super::super::range::decimal)
                    .transpose()?
                    .is_some_and(|n| n != count)
            {
                return Err(ErrorCode::RangeInvalid);
            }
            (Some(count), total, true)
        } else if raw.status == 200 && offset == 0 {
            if unique(&raw.headers, "content-range")?.is_some() {
                return Err(ErrorCode::RangeInvalid);
            }
            let size = unique(&raw.headers, "content-length")?
                .map(super::super::range::decimal)
                .transpose()?;
            let expected = match (size, target_len) {
                (Some(s), Some(l)) => Some(s.min(l)),
                (Some(s), None) => Some(s),
                (None, l) => l,
            };
            (expected, size, size.is_some())
        } else {
            let code = if matches!(raw.status, 401 | 403) {
                ErrorCode::AuthFailed
            } else {
                ErrorCode::IoError
            };
            logging::event(
                logging::EventLevel::Warn,
                "stream",
                "upstream_open_failed",
                json!({
                    "host": logging::url_host(&self.target.resource.url),
                    "offset": offset,
                    "status": raw.status,
                    "error": format!("{code:?}"),
                }),
            );
            return Err(code);
        };
        if offset > 0 && !retried {
            self.target.remember(&raw.headers, total, true)?;
        } else {
            self.target.remember_stream(&raw.headers, total, true)?;
        }

        // Pre-read the first chunk before committing loopback HTTP headers so
        // immediate upstream failures still yield a clean HTTP error status.
        let initial_cap = expected.map_or(STREAM_CHUNK, |n| (n as usize).min(STREAM_CHUNK));
        let mut first = vec![0u8; initial_cap];
        let mut used = 0usize;
        let mut eof = initial_cap == 0;
        let probe_min = initial_cap.min(64);
        while used < probe_min {
            let deadline = Instant::now() + STREAM_IDLE_TIMEOUT;
            let n = response.state.read(&mut first[used..], deadline)?;
            if n == 0 {
                eof = true;
                break;
            }
            used += n;
        }
        first.truncate(used);
        if eof && exact_length && expected.is_some_and(|n| (used as u64) < n) {
            return Err(ErrorCode::IoError);
        }
        let end = offset
            .checked_add(used as u64)
            .ok_or(ErrorCode::RangeInvalid)?;
        if total.is_some_and(|n| end > n) {
            return Err(ErrorCode::RangeInvalid);
        }
        let left = expected.map(|n| n.saturating_sub(used as u64));
        let done = eof || left == Some(0) || total == Some(end);
        let rest: Box<dyn StreamSlice> = if done {
            Box::new(EmptySlice)
        } else {
            Box::new(RemoteStream {
                target: Arc::clone(&self.target),
                response: Some(response),
                offset: end,
                remaining: left,
                total: total.or(known),
                exact_length,
                consecutive_resumes: 0,
                cancel: cancel.clone(),
            })
        };
        Ok(OpenedStream {
            first,
            eof: done,
            total,
            remaining: expected,
            rest,
        })
    }
}

impl RemoteTarget {
    /// Opens a GET range request, retrying once without `If-Range` if a
    /// multi-edge CDN rejects the pinned edge ETag with `412` or `200` (with
    /// matching byte length).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_range_with_fallback(
        &self,
        range: Option<(u64, Option<u64>)>,
        known_total: Option<u64>,
        allow_if_range: bool,
        retry_on_200: bool,
        host_bytes: u64,
        cancel: &HttpCancel,
        deadline: impl Fn() -> Instant,
    ) -> Result<(Response, bool), ErrorCode> {
        let mut send_if_range = range.is_some() && allow_if_range && self.has_etag();
        let mut retried = false;
        loop {
            let resp =
                self.open_range("GET", range, send_if_range, host_bytes, cancel, deadline())?;
            let RawState::Response(raw) = &resp.state else {
                return Err(ErrorCode::Internal);
            };
            if send_if_range {
                if raw.status == 412 {
                    send_if_range = false;
                    retried = true;
                    continue;
                }
                if raw.status == 200 && retry_on_200 {
                    let size = unique(&raw.headers, "content-length")?
                        .map(super::super::range::decimal)
                        .transpose()?;
                    if known_total.is_some() && size == known_total {
                        send_if_range = false;
                        retried = true;
                        continue;
                    }
                    return Err(ErrorCode::StaleHandle);
                }
            }
            return Ok((resp, retried));
        }
    }

    pub(super) fn remember_stream(
        &self,
        headers: &[(String, String)],
        size: Option<u64>,
        reset_etag: bool,
    ) -> Result<(), ErrorCode> {
        if self.verified {
            return self.remember(headers, size, false);
        }
        let etag = unique(headers, "etag")?
            .filter(|s| s.len() >= 2 && s.starts_with('"') && s.ends_with('"') && s.len() <= 1024)
            .map(str::to_owned);
        if self.expected.is_some() && size.is_some() && size != self.expected {
            return Err(ErrorCode::RangeInvalid);
        }
        let mut metadata = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = metadata.as_ref()
            && old.size.is_some()
            && size.is_some()
            && old.size != size
        {
            return Err(ErrorCode::StaleHandle);
        }
        let next_size = size.or_else(|| metadata.as_ref().and_then(|m| m.size));
        let next_etag = if reset_etag {
            etag
        } else {
            etag.or_else(|| metadata.as_ref().and_then(|m| m.etag.clone()))
        };
        *metadata = Some(Metadata {
            size: next_size,
            etag: next_etag,
        });
        Ok(())
    }
}
