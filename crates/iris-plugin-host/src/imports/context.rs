//! Store-level host state, accounted `Blob` buffers, and narrow host-service
//! callback interface shared by `imports`, `engine`, `instance`, and `actor`.
use crate::ledger::{Ledger, Reservation, Resource};
use iris_api::v2::{CHUNK_LIMIT, Capability, ErrorCode, Generation, Id, Role, task::Task};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::{CancelToken, Deadline, WaitError};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
    time::Instant,
};
use wasmtime::StoreLimits;

pub(crate) struct Blob {
    pub bytes: Vec<u8>,
    pub(crate) owner: String,
    _reservation: Reservation,
}

impl Blob {
    pub(crate) fn new(ledger: &Ledger, owner: &str, bytes: Vec<u8>) -> Result<Self, ErrorCode> {
        if bytes.len() > CHUNK_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let reservation = ledger.reserve(owner, &[(Resource::HostBytes, bytes.len() as u64)])?;
        Ok(Self {
            bytes,
            owner: owner.to_owned(),
            _reservation: reservation,
        })
    }
}

pub(crate) struct DetachedBlob {
    pub data: Arc<Blob>,
    pub(crate) _handle: Reservation,
}

impl DetachedBlob {
    pub(crate) fn reserve(
        ledger: &Ledger,
        owner: &str,
        data: Arc<Blob>,
    ) -> Result<Self, ErrorCode> {
        if data.owner != owner {
            return Err(ErrorCode::Denied);
        }
        Ok(Self {
            data,
            _handle: ledger.reserve(owner, &[(Resource::Handle, 1)])?,
        })
    }
}

pub(crate) trait HostCallbacks: Send {
    fn on_request(&mut self, cancel: Option<HttpCancel>);
    fn on_window(&mut self, window: Option<Id>);
    fn on_event(&mut self, generation: Generation, read: Option<Id>);
    fn submit(&mut self, task: Task, blobs: &BTreeMap<u64, Arc<Blob>>) -> Result<u64, ErrorCode>;
    fn close(&mut self, handle: u64) -> Result<(), ErrorCode>;
    fn reply(&mut self, request: u64, blob: Arc<Blob>, eof: bool) -> Result<usize, ErrorCode>;
    fn fail(&mut self, request: u64, error: ErrorCode) -> Result<(), ErrorCode>;
}

pub(crate) struct State {
    pub role: Role,
    pub grants: BTreeSet<Capability>,
    pub owner: String,
    pub ledger: Ledger,
    pub blobs: BTreeMap<u64, Arc<Blob>>,
    pub blob_handles: BTreeMap<u64, Reservation>,
    pub alloc_handle: fn() -> Result<u64, ErrorCode>,
    pub services: Option<Box<dyn HostCallbacks>>,
    pub cancel: CancelToken,
    pub started: Instant,
    pub deadline: Deadline,
    pub ready: bool,
    pub limits: StoreLimits,
    pub _memory: Reservation,
}

impl State {
    pub(crate) fn check_live(&self) -> Result<(), ErrorCode> {
        match self.cancel.check(Some(self.deadline)) {
            Ok(_) => Ok(()),
            Err(WaitError::Cancelled) => Err(ErrorCode::Cancelled),
            Err(WaitError::TimedOut) => Err(ErrorCode::Timeout),
        }
    }
}

pub(crate) fn checked_range(
    ptr: u32,
    len: u32,
    memory_len: usize,
    allow_empty: bool,
) -> Result<Range<usize>, ErrorCode> {
    if (len == 0 && !allow_empty) || (len != 0 && ptr == 0) {
        return Err(ErrorCode::InvalidArgument);
    }
    let end = (ptr as usize)
        .checked_add(len as usize)
        .ok_or(ErrorCode::RangeInvalid)?;
    if end > memory_len {
        return Err(ErrorCode::RangeInvalid);
    }
    Ok(ptr as usize..end)
}
