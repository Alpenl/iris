//! Host-fetched resources reuse the same request grant as guest networking.
//! Fallback bytes are verified against a fixed chunk-hash map before exposure.
mod document;
mod stream;
use super::reader::{OpenedStream, Reader, check_read};
use crate::{
    plugin_v2::{
        actor::ReadResult,
        instance::Blob,
        ledger::{Activity, Ledger, Reservation, Resource},
        network::{
            NetworkGrant, RequestGrant, TransportKind,
            cookies::CookieJar,
            normalize_url,
            raw::{self, RawRequest, RawState},
        },
    },
    vfs::HttpCancel,
};
pub(crate) use document::Fetched;
pub use document::Thumbnail;
use iris_api::v2::{
    CHUNK_LIMIT, ErrorCode,
    stream::{ByteIdentity, RemoteResource},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct RemoteFactory {
    grant: NetworkGrant,
    ledger: Ledger,
    cookies: Arc<CookieJar>,
    activity: Activity,
}
impl RemoteFactory {
    pub fn new(grant: NetworkGrant, ledger: Ledger) -> Result<Self, ErrorCode> {
        let cookies = Arc::new(CookieJar::new(grant.owner(), &ledger)?);
        Ok(Self {
            grant,
            ledger,
            cookies,
            activity: Activity::default(),
        })
    }
    pub fn owner(&self) -> &str {
        self.grant.owner()
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    pub fn is_quiescent(&self) -> bool {
        self.activity.is_idle()
    }
    pub fn reader(
        &self,
        resource: &RemoteResource,
        size: Option<u64>,
    ) -> Result<Arc<RemoteReader>, ErrorCode> {
        if resource.url.len() > 4096
            || resource.headers.len() > 64
            || resource
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                > 16 << 10
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        raw::validate_headers(&resource.headers)?;
        if resource.headers.iter().any(|(k, _)| {
            ["range", "if-range", "accept-encoding"]
                .iter()
                .any(|s| k.eq_ignore_ascii_case(s))
        }) {
            return Err(ErrorCode::Denied);
        }
        let grant = self.grant.request_tracked(
            &resource.url,
            TransportKind::Http,
            resource.origin_grant,
            self.activity.clone(),
        )?;
        grant.check_credentials(
            &normalize_url(&resource.url, TransportKind::Http)?,
            &resource.headers,
        )?;
        let retained = 4096
            + resource.url.len()
            + resource
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>();
        let memory = self
            .ledger
            .reserve(self.owner(), &[(Resource::HostBytes, retained as u64)])?;
        Ok(Arc::new(RemoteReader {
            factory: self.clone(),
            resource: resource.clone(),
            grant,
            expected: size,
            metadata: Arc::new(Mutex::new(None)),
            _memory: memory,
        }))
    }
    pub fn direct(
        &self,
        primary: &RemoteResource,
        fallback: &[RemoteResource],
        identity: Option<(&ByteIdentity, Arc<Blob>)>,
    ) -> Result<Arc<dyn Reader>, ErrorCode> {
        if fallback.len() > 8 || !fallback.is_empty() && identity.is_none() {
            return Err(ErrorCode::InvalidArgument);
        }
        let Some((identity, hashes)) = identity else {
            return Ok(self.reader(primary, None)?);
        };
        if hashes.owner != self.owner()
            || identity.size.0 == 0
            || !(65536..=CHUNK_LIMIT as u32).contains(&identity.chunk_bytes)
            || !identity.chunk_bytes.is_power_of_two()
            || identity
                .size
                .0
                .div_ceil(u64::from(identity.chunk_bytes))
                .checked_mul(32)
                != Some(hashes.bytes.len() as u64)
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let sources = std::iter::once(primary)
            .chain(fallback)
            .map(|s| self.reader(s, Some(identity.size.0)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(VerifiedReader {
            factory: self.clone(),
            sources,
            size: identity.size.0,
            chunk: identity.chunk_bytes,
            hashes,
            preferred: AtomicUsize::new(0),
        }))
    }
}
#[derive(Clone)]
struct Metadata {
    size: Option<u64>,
    etag: Option<String>,
}
#[derive(Clone)]
struct RemoteTarget {
    factory: RemoteFactory,
    resource: RemoteResource,
    grant: Arc<RequestGrant>,
    expected: Option<u64>,
    metadata: Arc<Mutex<Option<Metadata>>>,
}
impl RemoteTarget {
    fn open_range(
        &self,
        method: &str,
        range: Option<(u64, Option<u64>)>,
        send_if_range: bool,
        host_bytes: u64,
        cancel: &HttpCancel,
        deadline: Instant,
    ) -> Result<Response, ErrorCode> {
        self.grant.check_live()?;
        let memory = self.factory.ledger.reserve(
            self.factory.owner(),
            &[(Resource::Http, 1), (Resource::HostBytes, host_bytes)],
        )?;
        let mut headers = self.resource.headers.clone();
        if let Some((start, end)) = range {
            let value = match end {
                Some(end) => format!("bytes={start}-{end}"),
                None => format!("bytes={start}-"),
            };
            headers.push(("Range".into(), value));
            if send_if_range
                && let Some(etag) = self
                    .metadata
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|m| m.etag.clone())
            {
                headers.push(("If-Range".into(), etag));
            }
        }
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ErrorCode::Timeout)?;
        let cancel = cancel.child().map_err(|_| ErrorCode::Cancelled)?;
        let cookies = self
            .grant
            .cookies()
            .unwrap_or_else(|| self.factory.cookies.clone());
        let state = raw::open(
            RawRequest {
                method: method.into(),
                url: normalize_url(&self.resource.url, TransportKind::Http)?,
                headers,
                follow_redirects: true,
                timeout,
                cookies: Some(cookies),
            },
            None,
            false,
            self.grant.clone(),
            cancel.clone(),
        )?;
        Ok(Response {
            state,
            _memory: memory,
            cancel,
        })
    }
}
pub struct RemoteReader {
    factory: RemoteFactory,
    resource: RemoteResource,
    grant: Arc<RequestGrant>,
    expected: Option<u64>,
    metadata: Arc<Mutex<Option<Metadata>>>,
    _memory: Reservation,
}
struct Response {
    state: RawState,
    _memory: Reservation,
    cancel: HttpCancel,
}
impl Drop for Response {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl RemoteReader {
    fn target(&self) -> RemoteTarget {
        RemoteTarget {
            factory: self.factory.clone(),
            resource: self.resource.clone(),
            grant: self.grant.clone(),
            expected: self.expected,
            metadata: self.metadata.clone(),
        }
    }
    fn open_range(
        &self,
        method: &str,
        range: Option<(u64, Option<u64>)>,
        send_if_range: bool,
        host_bytes: u64,
        cancel: &HttpCancel,
        deadline: Instant,
    ) -> Result<Response, ErrorCode> {
        self.target()
            .open_range(method, range, send_if_range, host_bytes, cancel, deadline)
    }
    fn open(
        &self,
        method: &str,
        range: Option<(u64, u64)>,
        cancel: &HttpCancel,
        deadline: Instant,
    ) -> Result<Response, ErrorCode> {
        let range = range
            .map(|(start, len)| {
                let end = start
                    .checked_add(len.checked_sub(1).ok_or(ErrorCode::RangeInvalid)?)
                    .ok_or(ErrorCode::RangeInvalid)?;
                Ok::<_, ErrorCode>((start, Some(end)))
            })
            .transpose()?;
        self.open_range(method, range, true, 2 << 20, cancel, deadline)
    }
    fn remember(
        &self,
        headers: &[(String, String)],
        size: Option<u64>,
        require_etag_continuity: bool,
    ) -> Result<(), ErrorCode> {
        let etag = unique(headers, "etag")?
            .filter(|s| s.len() >= 2 && s.starts_with('"') && s.ends_with('"') && s.len() <= 1024)
            .map(str::to_owned);
        if self.expected.is_some() && size.is_some() && size != self.expected {
            return Err(ErrorCode::RangeInvalid);
        }
        let mut metadata = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = metadata.as_ref()
            && (old.size.is_some() && size.is_some() && old.size != size
                || old.etag.is_some()
                    && (require_etag_continuity || etag.is_some())
                    && old.etag != etag)
        {
            return Err(ErrorCode::StaleHandle);
        }
        let size = size.or_else(|| metadata.as_ref().and_then(|m| m.size));
        let etag = etag.or_else(|| {
            if require_etag_continuity {
                None
            } else {
                metadata.as_ref().and_then(|m| m.etag.clone())
            }
        });
        *metadata = Some(Metadata { size, etag });
        Ok(())
    }
    fn read_until(
        &self,
        offset: u64,
        len: u32,
        cancel: &HttpCancel,
        deadline: Instant,
    ) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        self.grant.check_live()?;
        let known = self.expected.or_else(|| {
            self.metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.size)
        });
        if known.is_some_and(|size| offset > size) {
            return Err(ErrorCode::RangeInvalid);
        }
        if known == Some(offset) {
            return self.blob(Vec::new(), true);
        }
        let len = known.map_or(u64::from(len), |size| (size - offset).min(u64::from(len)));
        let mut response = self.open("GET", Some((offset, len)), cancel, deadline)?;
        let RawState::Response(raw) = &response.state else {
            return Err(ErrorCode::Internal);
        };
        identity_encoding(&raw.headers)?;
        if raw.status == 416 {
            let total = unique(&raw.headers, "content-range")?
                .and_then(|v| v.strip_prefix("bytes */"))
                .ok_or(ErrorCode::RangeInvalid)?;
            let total = super::range::decimal(total)?;
            self.remember(&raw.headers, Some(total), false)?;
            if offset != total {
                return Err(ErrorCode::RangeInvalid);
            }
            return self.blob(Vec::new(), true);
        }
        let (expected, total) = if raw.status == 206 {
            let (start, count, total) = content_range(
                unique(&raw.headers, "content-range")?.ok_or(ErrorCode::RangeInvalid)?,
            )?;
            if start != offset
                || count > len
                || unique(&raw.headers, "content-length")?
                    .map(super::range::decimal)
                    .transpose()?
                    .is_some_and(|n| n != count)
            {
                return Err(ErrorCode::RangeInvalid);
            }
            (count, total)
        } else if raw.status == 200 && offset == 0 {
            if unique(&raw.headers, "content-range")?.is_some() {
                return Err(ErrorCode::RangeInvalid);
            }
            let size = unique(&raw.headers, "content-length")?
                .map(super::range::decimal)
                .transpose()?;
            (size.map_or(len, |n| n.min(len)), size)
        } else {
            return Err(if matches!(raw.status, 401 | 403) {
                ErrorCode::AuthFailed
            } else {
                ErrorCode::IoError
            });
        };
        self.remember(&raw.headers, total, true)?;
        let whole = raw.status == 200;
        let mut bytes = vec![0; expected as usize];
        let mut used = 0;
        let mut eof = false;
        while used < bytes.len() {
            let n = response.state.read(&mut bytes[used..], deadline)?;
            if n == 0 {
                eof = true;
                break;
            }
            used += n;
        }
        if used < expected as usize && (!whole || total.is_some()) {
            return Err(ErrorCode::IoError);
        }
        bytes.truncate(used);
        let end = offset
            .checked_add(used as u64)
            .ok_or(ErrorCode::RangeInvalid)?;
        if total.is_some_and(|n| end > n) || used == 0 && !eof && total != Some(end) {
            return Err(ErrorCode::RangeInvalid);
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        self.grant.check_live()?;
        self.blob(bytes, total == Some(end) || whole && eof)
    }
    fn blob(&self, bytes: Vec<u8>, eof: bool) -> Result<ReadResult, ErrorCode> {
        Ok(ReadResult {
            blob: Arc::new(Blob::new(
                &self.factory.ledger,
                self.factory.owner(),
                bytes,
            )?),
            eof,
        })
    }
}
impl Reader for RemoteReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        self.grant.check_live()?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if let Some(size) = self.expected {
            return Ok(Some(size));
        }
        if let Some(metadata) = self
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Ok(metadata.size);
        }
        let response = self.open(
            "HEAD",
            None,
            cancel,
            Instant::now() + Duration::from_secs(30),
        )?;
        let RawState::Response(raw) = &response.state else {
            return Err(ErrorCode::Internal);
        };
        if raw.status == 405 || raw.status == 501 {
            return Ok(None);
        }
        if raw.status != 200 {
            return Err(ErrorCode::IoError);
        }
        identity_encoding(&raw.headers)?;
        let size = unique(&raw.headers, "content-length")?
            .map(super::range::decimal)
            .transpose()?;
        self.remember(&raw.headers, size, true)?;
        Ok(size)
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        self.read_until(
            offset,
            len,
            cancel,
            Instant::now() + Duration::from_secs(30),
        )
    }
    fn open_stream(
        &self,
        offset: u64,
        len: Option<u64>,
        cancel: &HttpCancel,
    ) -> Result<Option<OpenedStream>, ErrorCode> {
        self.open_continuous(offset, len, cancel).map(Some)
    }
}
struct VerifiedReader {
    factory: RemoteFactory,
    sources: Vec<Arc<RemoteReader>>,
    size: u64,
    chunk: u32,
    hashes: Arc<Blob>,
    preferred: AtomicUsize,
}
impl Reader for VerifiedReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        for source in &self.sources {
            source.grant.check_live()?;
        }
        Ok(Some(self.size))
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        if offset > self.size {
            return Err(ErrorCode::RangeInvalid);
        }
        if offset == self.size {
            return Ok(ReadResult {
                blob: Arc::new(Blob::new(
                    &self.factory.ledger,
                    self.factory.owner(),
                    Vec::new(),
                )?),
                eof: true,
            });
        }
        let index = offset / u64::from(self.chunk);
        let start = index * u64::from(self.chunk);
        let chunk_len = u64::from(self.chunk).min(self.size - start) as usize;
        let _scratch = self.factory.ledger.reserve(
            self.factory.owner(),
            &[(Resource::HostBytes, (CHUNK_LIMIT * 2) as u64)],
        )?;
        let overall_deadline = Instant::now() + Duration::from_secs(45);
        let preferred = self.preferred.load(Ordering::Relaxed);
        let mut error = ErrorCode::IoError;
        for attempt in 0..self.sources.len() {
            let attempt_deadline = overall_deadline.min(Instant::now() + Duration::from_secs(15));
            let source = (preferred + attempt) % self.sources.len();
            let mut bytes = Vec::with_capacity(chunk_len);
            let result = (|| {
                while bytes.len() < chunk_len {
                    let read = self.sources[source].read_until(
                        start + bytes.len() as u64,
                        (chunk_len - bytes.len()) as u32,
                        cancel,
                        attempt_deadline,
                    )?;
                    if read.blob.bytes.is_empty() {
                        return Err(ErrorCode::RangeInvalid);
                    }
                    bytes.extend_from_slice(&read.blob.bytes);
                }
                let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
                let hash = index as usize * 32;
                if digest.as_ref() != &self.hashes.bytes[hash..hash + 32] {
                    return Err(ErrorCode::AuthFailed);
                }
                Ok(())
            })();
            if let Err(code) = result {
                if cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
                error = code;
                continue;
            }
            self.preferred.store(source, Ordering::Relaxed);
            let begin = (offset - start) as usize;
            let end = chunk_len.min(begin + len as usize);
            let data = bytes[begin..end].to_vec();
            return Ok(ReadResult {
                blob: Arc::new(Blob::new(&self.factory.ledger, self.factory.owner(), data)?),
                eof: offset + (end - begin) as u64 == self.size,
            });
        }
        Err(error)
    }
}
pub(crate) fn unique<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, ErrorCode> {
    let mut values = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name));
    let value = values.next().map(|(_, v)| v.as_str());
    if values.next().is_some() {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(value)
}
fn identity_encoding(headers: &[(String, String)]) -> Result<(), ErrorCode> {
    if unique(headers, "content-encoding")?.is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Err(ErrorCode::Unsupported);
    }
    Ok(())
}
fn content_range(value: &str) -> Result<(u64, u64, Option<u64>), ErrorCode> {
    let (range, total) = value
        .strip_prefix("bytes ")
        .and_then(|v| v.split_once('/'))
        .ok_or(ErrorCode::RangeInvalid)?;
    let (start, end) = range.split_once('-').ok_or(ErrorCode::RangeInvalid)?;
    let start = super::range::decimal(start)?;
    let end = super::range::decimal(end)?;
    let total = if total == "*" {
        None
    } else {
        Some(super::range::decimal(total)?)
    };
    if total.is_some_and(|n| end >= n) {
        return Err(ErrorCode::RangeInvalid);
    }
    Ok((
        start,
        end.checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or(ErrorCode::RangeInvalid)?,
        total,
    ))
}
