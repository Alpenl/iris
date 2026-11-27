use crate::{
    actor::ReadResult,
    instance::Blob,
    ledger::{Ledger, Reservation, Resource},
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
use iris_base::net::http_client::HttpCancel;
use std::sync::Arc;

/// Incremental upstream stream owned by one loopback relay response.
pub trait StreamSlice: Send {
    fn read_chunk(&mut self, out: &mut [u8]) -> Result<usize, ErrorCode>;
}

/// Pre-validated stream opened before committing loopback HTTP headers.
pub struct OpenedStream {
    pub first: Vec<u8>,
    pub eof: bool,
    pub total: Option<u64>,
    pub remaining: Option<u64>,
    pub rest: Box<dyn StreamSlice>,
}

/// Runs only on relay/file workers. The Store is never held during this wait.
pub trait Reader: Send + Sync {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode>;
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode>;
    fn open_stream(
        &self,
        _offset: u64,
        _len: Option<u64>,
        _cancel: &HttpCancel,
    ) -> Result<Option<OpenedStream>, ErrorCode> {
        Ok(None)
    }
}
pub(crate) fn check_read(offset: u64, len: u32, cancel: &HttpCancel) -> Result<(), ErrorCode> {
    if cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    if len == 0 || len as usize > CHUNK_LIMIT || offset.checked_add(u64::from(len)).is_none() {
        return Err(ErrorCode::RangeInvalid);
    }
    Ok(())
}
pub struct MemoryReader {
    bytes: Vec<u8>,
    owner: String,
    ledger: Ledger,
    _memory: Reservation,
}
impl MemoryReader {
    /// Caller reserves parse/render scratch before building these bytes. The
    /// resident allocation gets its own lease before that scratch is released.
    pub fn new(bytes: Vec<u8>, owner: &str, ledger: &Ledger) -> Result<Self, ErrorCode> {
        if bytes.len() > super::manifest::OUTPUT_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let memory = ledger.reserve(owner, &[(Resource::HostBytes, bytes.len() as u64)])?;
        Ok(Self {
            bytes,
            owner: owner.into(),
            ledger: ledger.clone(),
            _memory: memory,
        })
    }
}
impl Reader for MemoryReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(Some(self.bytes.len() as u64))
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        let start = usize::try_from(offset).map_err(|_| ErrorCode::RangeInvalid)?;
        if start > self.bytes.len() {
            return Err(ErrorCode::RangeInvalid);
        }
        let end = self.bytes.len().min(start + len as usize);
        // Reserve the temporary copy before allocation; Blob retains its own
        // reservation once constructed.
        let _scratch = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, (end - start) as u64)])?;
        let blob = Blob::new(&self.ledger, &self.owner, self.bytes[start..end].to_vec())?;
        Ok(ReadResult {
            blob: Arc::new(blob),
            eof: end == self.bytes.len(),
        })
    }
}
