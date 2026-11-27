pub(crate) use super::imports::{Blob, DetachedBlob};
use super::{
    engine::{CompiledModule, RuntimeEngine},
    imports::{HostCallbacks, State, checked_range},
    json_error,
    ledger::{Ledger, Reservation, Resource},
};
use iris_api::v2::{
    COMMAND_LIMIT, Capability, ErrorCode, HostCommand, Id, MESSAGE_LIMIT, Reply, Role, unpack,
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::{CancelToken, Deadline};
use iris_core::bounded_json::{decode, decode_exact};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use wasmtime::{Memory, Store, StoreLimitsBuilder, TypedFunc, UpdateDeadline};
mod contexts;

/// Sole process-wide static in `iris-plugin-host`: a stateless monotonic counter
/// allocating non-reused `1..=i64::MAX` handle/operation/session IDs across the
/// process lifetime so numeric handles can never collide across retired or
/// concurrent `PluginHost` / `Store` instances. It holds zero host, quota, or
/// authorization state; every lookup validates ownership, `Binding`, and
/// `Generation` against the owning `Store` / `PluginHost` instance.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
pub(crate) fn next_id() -> Result<u64, ErrorCode> {
    NEXT_HANDLE
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n <= i64::MAX as u64).then(|| n + 1)
        })
        .map_err(|_| ErrorCode::QuotaExceeded)
}

pub(crate) struct InstanceConfig {
    pub engine: RuntimeEngine,
    pub module: Arc<CompiledModule>,
    pub role: Role,
    pub owner: String,
    pub grants: BTreeSet<Capability>,
    pub ledger: Ledger,
    pub cancel: CancelToken,
}

pub(crate) struct Instance {
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
        if let Some(services) = self.store.data_mut().services.as_mut() {
            services.on_request(cancel);
        }
    }
    pub(crate) fn begin_event(&mut self, generation: iris_api::v2::Generation, read: Option<Id>) {
        if let Some(services) = self.store.data_mut().services.as_mut() {
            services.on_event(generation, read);
        }
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

    pub(crate) fn new(config: InstanceConfig) -> Result<Self, ErrorCode> {
        let reservation = Self::reserve(&config.ledger, &config.owner, config.role)?;
        Self::with_reservation(config, None, reservation)
    }

    pub(crate) fn with_reservation(
        config: InstanceConfig,
        services: Option<Box<dyn HostCallbacks>>,
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
            alloc_handle: next_id,
            services,
            cancel,
            started: now,
            deadline: Deadline::at(now + Duration::from_secs(5)),
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
        let _execution = engine.execution()?;
        let instance = engine
            .linker()
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

    pub(crate) fn deliver_blob(&mut self, blob: Arc<Blob>) -> Result<u64, ErrorCode> {
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

    pub(crate) fn on_event(&mut self, input: &[u8]) -> Result<Reply<HostCommand>, ErrorCode> {
        self.call(input, Duration::from_secs(2))
    }
    pub(crate) fn detach_blobs(
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
        decode::<serde_json::Value>(input, MESSAGE_LIMIT).map_err(json_error)?;
        if input.is_empty() {
            return Err(ErrorCode::InvalidArgument);
        }
        let _scratch = self.store.data().ledger.reserve(
            &self.store.data().owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 2) as u64)],
        )?;
        self.store.data_mut().deadline = Deadline::after(budget);
        self.store.data().check_live()?;
        self.store.set_epoch_deadline(1);
        let _execution = self.engine.execution()?;
        let result = self.invoke(input).and_then(|bytes| {
            if bytes.is_empty() {
                return Ok(Reply::Ok { ok: Vec::new() });
            }
            let reply: Reply<HostCommand> =
                decode_exact(&bytes, MESSAGE_LIMIT).map_err(json_error)?;
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

#[cfg(test)]
mod tests;
