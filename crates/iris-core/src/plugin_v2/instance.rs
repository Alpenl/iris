use super::{
    engine::{CompiledModule, RuntimeEngine},
    ledger::{Ledger, Reservation, Resource},
};
use crate::vfs::HttpCancel;
use iris_api::v2::{
    CHUNK_LIMIT, COMMAND_LIMIT, Capability, ErrorCode, HostCommand, Id, MESSAGE_LIMIT, Reply, Role,
    task::Task, unpack,
};
use crate::plugin_v2::json::decode;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use wasmtime::{Memory, Store, StoreLimits, StoreLimitsBuilder, TypedFunc, UpdateDeadline};
mod contexts;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
pub(crate) fn next_id() -> Result<u64, ErrorCode> {
    NEXT_HANDLE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n <= i64::MAX as u64).then(|| n + 1)
        })
        .map_err(|_| ErrorCode::QuotaExceeded)
}

pub struct Blob {
    pub bytes: Vec<u8>,
    pub(crate) owner: String,
    _reservation: Reservation,
}
pub struct DetachedBlob {
    pub data: Arc<Blob>,
    _handle: Reservation,
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
impl Blob {
    pub fn new(ledger: &Ledger, owner: &str, bytes: Vec<u8>) -> Result<Self, ErrorCode> {
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

/// Implementations only validate and enqueue. I/O must never run from an import.
pub trait Services: Send {
    fn on_window(&mut self, _window: Option<Id>) {}
    fn on_request(&mut self, _cancel: Option<HttpCancel>) {}
    fn on_event(&mut self, _generation: iris_api::v2::Generation, _read: Option<Id>) {}
    fn submit(&mut self, task: Task, blobs: &BTreeMap<u64, Arc<Blob>>) -> Result<u64, ErrorCode>;
    fn close(&mut self, handle: u64) -> Result<(), ErrorCode>;
    fn reply(&mut self, request: u64, blob: Arc<Blob>, eof: bool) -> Result<usize, ErrorCode>;
    fn fail(&mut self, request: u64, error: ErrorCode) -> Result<(), ErrorCode>;
}

pub struct NoServices;
impl Services for NoServices {
    fn submit(&mut self, _: Task, _: &BTreeMap<u64, Arc<Blob>>) -> Result<u64, ErrorCode> {
        Err(ErrorCode::Denied)
    }
    fn close(&mut self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn reply(&mut self, _: u64, _: Arc<Blob>, _: bool) -> Result<usize, ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn fail(&mut self, _: u64, _: ErrorCode) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
}

pub(crate) struct State {
    pub role: Role,
    pub grants: BTreeSet<Capability>,
    pub owner: String,
    pub ledger: Ledger,
    pub blobs: BTreeMap<u64, Arc<Blob>>,
    pub blob_handles: BTreeMap<u64, Reservation>,
    pub services: Box<dyn Services>,
    pub cancel: Arc<AtomicBool>,
    pub started: Instant,
    pub deadline: Instant,
    pub ready: bool,
    limits: StoreLimits,
    _memory: Reservation,
}

impl State {
    pub fn check_live(&self) -> Result<(), ErrorCode> {
        if self.cancel.load(Ordering::Acquire) {
            Err(ErrorCode::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(ErrorCode::Timeout)
        } else {
            Ok(())
        }
    }
}

pub struct InstanceConfig {
    pub engine: RuntimeEngine,
    pub module: Arc<CompiledModule>,
    pub role: Role,
    pub owner: String,
    pub grants: BTreeSet<Capability>,
    pub ledger: Ledger,
    pub cancel: Arc<AtomicBool>,
    pub services: Box<dyn Services>,
}

pub struct Instance {
    engine: RuntimeEngine,
    _module: Arc<CompiledModule>,
    store: Store<State>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    dealloc: TypedFunc<(i32, i32), ()>,
    event: TypedFunc<(i32, i32), i64>,
    healthy: bool,
    current_context: (Option<Id>, iris_api::v2::Generation),
    parked: BTreeMap<Option<Id>, contexts::BlobContext>,
}

impl Instance {
    pub(crate) fn request_scope(&mut self, cancel: Option<HttpCancel>) {
        self.store.data_mut().services.on_request(cancel);
    }
    pub(crate) fn begin_event(&mut self, generation: iris_api::v2::Generation, read: Option<Id>) {
        self.store.data_mut().services.on_event(generation, read);
    }
    pub(crate) fn adopt_blob(&mut self, id: Id, blob: DetachedBlob) -> Result<(), ErrorCode> {
        if !self.healthy {
            return Err(ErrorCode::BadState);
        }
        let state = self.store.data_mut();
        if state.owner != blob.data.owner {
            return Err(ErrorCode::Denied);
        }
        if state.blobs.contains_key(&id.0) || state.blob_handles.contains_key(&id.0) {
            return Err(ErrorCode::BadState);
        }
        state.blobs.insert(id.0, blob.data);
        state.blob_handles.insert(id.0, blob._handle);
        Ok(())
    }
    pub(crate) fn reserve(
        ledger: &Ledger,
        owner: &str,
        role: Role,
    ) -> Result<Reservation, ErrorCode> {
        ledger.reserve(
            owner,
            &[
                (
                    Resource::WasmBytes,
                    if role == Role::Control {
                        128 << 20
                    } else {
                        64 << 20
                    },
                ),
                (
                    if role == Role::Control {
                        Resource::ControlStore
                    } else {
                        Resource::StreamStore
                    },
                    1,
                ),
            ],
        )
    }

    pub fn new(config: InstanceConfig) -> Result<Self, ErrorCode> {
        let reservation = Self::reserve(&config.ledger, &config.owner, config.role)?;
        Self::with_reservation(config, reservation)
    }

    pub(crate) fn with_reservation(
        config: InstanceConfig,
        reservation: Reservation,
    ) -> Result<Self, ErrorCode> {
        let InstanceConfig {
            engine,
            module,
            role,
            owner,
            grants,
            ledger,
            cancel,
            services,
        } = config;
        let memory_limit = if role == Role::Control {
            128 << 20
        } else {
            64 << 20
        };
        let now = Instant::now();
        let state = State {
            role,
            grants,
            owner,
            ledger,
            blobs: BTreeMap::new(),
            blob_handles: BTreeMap::new(),
            services,
            cancel,
            started: now,
            deadline: now + Duration::from_secs(5),
            ready: false,
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_limit as usize)
                .table_elements(16384)
                .instances(1)
                .tables(1)
                .memories(1)
                .build(),
            _memory: reservation,
        };
        state.check_live()?;
        let mut store = Store::new(engine.engine(), state);
        store.limiter(|s| &mut s.limits);
        store.epoch_deadline_callback(|cx| {
            cx.data()
                .check_live()
                .map_err(|_| wasmtime::Error::msg("guest execution cancelled or expired"))?;
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let linker = super::imports::linker(engine.engine())?;
        let _execution = engine.execution();
        let instance = linker
            .instantiate(&mut store, &module.module)
            .map_err(|_| {
                store
                    .data()
                    .check_live()
                    .err()
                    .unwrap_or(ErrorCode::InvalidArgument)
            })?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(ErrorCode::InvalidArgument)?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, "iris_abi_version")
            .map_err(|_| ErrorCode::InvalidArgument)?;
        if abi.call(&mut store, ()).map_err(|_| {
            store
                .data()
                .check_live()
                .err()
                .unwrap_or(ErrorCode::InvalidArgument)
        })? != 2
        {
            return Err(ErrorCode::Unsupported);
        }
        let alloc = instance
            .get_typed_func(&mut store, "iris_alloc")
            .map_err(|_| ErrorCode::InvalidArgument)?;
        let dealloc = instance
            .get_typed_func(&mut store, "iris_dealloc")
            .map_err(|_| ErrorCode::InvalidArgument)?;
        let event = instance
            .get_typed_func(&mut store, "iris_on_event")
            .map_err(|_| ErrorCode::InvalidArgument)?;
        store.data_mut().ready = true;
        Ok(Self {
            engine,
            _module: module,
            store,
            memory,
            alloc,
            dealloc,
            event,
            healthy: true,
            current_context: (None, Default::default()),
            parked: BTreeMap::new(),
        })
    }

    pub fn deliver_blob(&mut self, blob: Arc<Blob>) -> Result<u64, ErrorCode> {
        if !self.healthy {
            return Err(ErrorCode::BadState);
        }
        if blob.owner != self.store.data().owner {
            return Err(ErrorCode::Denied);
        }
        let reservation = self
            .store
            .data()
            .ledger
            .reserve(&self.store.data().owner, &[(Resource::Handle, 1)])?;
        let id = next_id()?;
        self.store.data_mut().blobs.insert(id, blob);
        self.store.data_mut().blob_handles.insert(id, reservation);
        Ok(id)
    }

    pub fn on_event(&mut self, input: &[u8]) -> Result<Reply<HostCommand>, ErrorCode> {
        self.call(input, Duration::from_secs(2))
    }
    pub fn detach_blobs(
        &mut self,
        ids: &BTreeSet<Id>,
    ) -> Result<BTreeMap<Id, DetachedBlob>, ErrorCode> {
        if !self.healthy {
            return Err(ErrorCode::BadState);
        }
        let state = self.store.data_mut();
        if ids
            .iter()
            .any(|id| !state.blobs.contains_key(&id.0) || !state.blob_handles.contains_key(&id.0))
        {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(ids
            .iter()
            .map(|id| {
                (
                    *id,
                    DetachedBlob {
                        data: state.blobs.remove(&id.0).unwrap(),
                        _handle: state.blob_handles.remove(&id.0).unwrap(),
                    },
                )
            })
            .collect())
    }

    pub(crate) fn call(
        &mut self,
        input: &[u8],
        budget: Duration,
    ) -> Result<Reply<HostCommand>, ErrorCode> {
        if !self.healthy {
            return Err(ErrorCode::BadState);
        }
        decode::<serde_json::Value>(input, MESSAGE_LIMIT)?;
        if input.is_empty() {
            return Err(ErrorCode::InvalidArgument);
        }
        let _scratch = self.store.data().ledger.reserve(
            &self.store.data().owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 2) as u64)],
        )?;
        self.store.data_mut().deadline = Instant::now() + budget;
        self.store.data().check_live()?;
        self.store.set_epoch_deadline(1);
        let _execution = self.engine.execution();
        let result = self.invoke(input).and_then(|bytes| {
            if bytes.is_empty() {
                return Ok(Reply::Ok { ok: Vec::new() });
            }
            let reply: Reply<HostCommand> =
                crate::plugin_v2::json::decode_exact(&bytes, MESSAGE_LIMIT)?;
            match &reply {
                Reply::Ok { ok } if ok.len() > COMMAND_LIMIT => {
                    return Err(ErrorCode::QuotaExceeded);
                }
                Reply::Error { error }
                    if ErrorCode::from_i32(error.code).is_none() || error.message.len() > 1024 =>
                {
                    return Err(ErrorCode::InvalidArgument);
                }
                _ => {}
            }
            Ok(reply)
        });
        if result.is_err() {
            self.healthy = false;
        }
        result
    }

    fn invoke(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ErrorCode> {
        let input_len = bytes.len() as u32;
        let input_ptr = self
            .alloc
            .call(&mut self.store, input_len as i32)
            .map_err(|_| self.trap_code())? as u32;
        let input_range = checked_range(
            input_ptr,
            input_len,
            self.memory.data_size(&self.store),
            false,
        )?;
        self.memory
            .write(&mut self.store, input_range.start, bytes)
            .map_err(|_| ErrorCode::InvalidArgument)?;
        // No borrowed Memory data crosses the guest callback (which may grow it).
        let packed = self
            .event
            .call(&mut self.store, (input_ptr as i32, input_len as i32))
            .map_err(|_| self.trap_code())? as u64;
        let output = if packed == 0 {
            Vec::new()
        } else {
            let (ptr, len) = unpack(packed);
            if len as usize > MESSAGE_LIMIT {
                return Err(ErrorCode::QuotaExceeded);
            }
            let range = checked_range(ptr, len, self.memory.data_size(&self.store), false)?;
            if range.start < input_range.end && input_range.start < range.end {
                return Err(ErrorCode::InvalidArgument);
            }
            let mut output = vec![0; len as usize];
            self.memory
                .read(&self.store, range.start, &mut output)
                .map_err(|_| ErrorCode::InvalidArgument)?;
            self.dealloc
                .call(&mut self.store, (ptr as i32, len as i32))
                .map_err(|_| self.trap_code())?;
            output
        };
        self.dealloc
            .call(&mut self.store, (input_ptr as i32, input_len as i32))
            .map_err(|_| self.trap_code())?;
        Ok(output)
    }

    fn trap_code(&self) -> ErrorCode {
        self.store
            .data()
            .check_live()
            .err()
            .unwrap_or(ErrorCode::InvalidArgument)
    }
}

pub(crate) fn checked_range(
    ptr: u32,
    len: u32,
    memory_len: usize,
    allow_empty: bool,
) -> Result<std::ops::Range<usize>, ErrorCode> {
    if (!allow_empty && (ptr == 0 || len == 0)) || (len != 0 && ptr == 0) {
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

#[cfg(test)]
mod tests;
