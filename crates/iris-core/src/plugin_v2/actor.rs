//! One owning thread per bounded Store. No UI caller joins a Store or I/O job.
mod bridge;
mod mailbox;
mod operation;
mod read;
pub(crate) use bridge::Bridge;
pub use operation::{Completion, Operation};
use read::ReadSlot;
pub use read::{ReadResult, ReadTicket};

use super::{
    engine::{CompiledModule, RuntimeEngine},
    instance::{Blob, DetachedBlob, Instance, InstanceConfig, next_id},
    json_error,
    ledger::{Ledger, Reservation, Resource},
    pool::IoPool,
};
use crate::vfs::http_client::HttpCancel;
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostCommand, Id, MESSAGE_LIMIT, Reply, Role, task::Task,
};
use mailbox::{Mailbox, Message};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub struct TaskContext {
    pub instance_id: Id,
    pub window: Option<Id>,
    pub owner: String,
    pub role: Role,
    pub generation: Generation,
    pub cancel: HttpCancel,
    pub ledger: Ledger,
}

pub struct IoOutcome {
    pub value: serde_json::Value,
    pub blob: Option<Arc<Blob>>,
    pub(crate) pending: Option<Box<dyn Send>>,
    discard: Option<Discard>,
}
impl IoOutcome {
    pub fn new(value: serde_json::Value, blob: Option<Arc<Blob>>) -> Self {
        Self {
            value,
            blob,
            pending: None,
            discard: None,
        }
    }
    pub fn on_discard(mut self, cleanup: impl FnOnce() + Send + 'static) -> Self {
        self.discard = Some(Discard(Some(Box::new(cleanup))));
        self
    }
    pub(crate) fn acknowledge(&mut self) {
        if let Some(mut discard) = self.discard.take() {
            discard.0 = None;
        }
    }
}
struct Discard(Option<Box<dyn FnOnce() + Send>>);
impl Drop for Discard {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

/// prepare/close only perform bounded validation and state transitions. The
/// returned operation owns all reservations until I/O and delivery are finished.
pub trait Executor: Send + Sync {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode>;
    fn close(&self, handle: u64) -> Result<(), ErrorCode>;
    fn close_in(&self, window: Option<Id>, handle: u64) -> Result<(), ErrorCode> {
        let _ = window;
        self.close(handle)
    }
    fn stop(&self);
    fn generation_changed(&self, _generation: Generation) {}
    fn window_changed(&self, _window: Id, _generation: Option<Generation>) {}
    fn is_quiescent(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Initializing,
    Ready,
    Stopping,
    Stopped,
    Failed(ErrorCode),
}

pub struct Update {
    pub commands: Vec<HostCommand>,
    pub blobs: BTreeMap<Id, DetachedBlob>,
    pub generation: Generation,
    pub window: Option<Id>,
    request_cancel: Option<HttpCancel>,
    _memory: Reservation,
}
impl Update {
    pub fn is_cancelled(&self) -> bool {
        self.request_cancel
            .as_ref()
            .is_some_and(HttpCancel::is_cancelled)
    }
}

#[derive(Default)]
struct UpdateQueue {
    items: VecDeque<Update>,
    closed: bool,
}

type SharedUpdates = Arc<(Mutex<UpdateQueue>, Condvar)>;

pub struct Actor {
    shared: Arc<Mailbox>,
    updates: SharedUpdates,
}

pub struct Spawn {
    pub engine: RuntimeEngine,
    pub module: Arc<CompiledModule>,
    pub role: Role,
    pub owner: String,
    pub grants: BTreeSet<Capability>,
    pub ledger: Ledger,
    pub io: IoPool,
    pub executor: Arc<dyn Executor>,
    pub generation: Generation,
    /// Called after state/output changes; it must only wake the UI/event loop.
    pub wake: Arc<dyn Fn() + Send + Sync>,
}

impl Actor {
    pub fn spawn(config: Spawn) -> Result<Self, ErrorCode> {
        let reservation = Instance::reserve(&config.ledger, &config.owner, config.role)?;
        let shared = Arc::new(Mailbox::new(
            config.owner.clone(),
            config.role,
            config.generation,
            config.grants.clone(),
            config.ledger.clone(),
            config.executor.clone(),
            config.wake,
        )?);
        let updates: SharedUpdates = Arc::new((Mutex::new(UpdateQueue::default()), Condvar::new()));
        let thread_updates = updates.clone();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name(format!("iris-v2-{:?}", config.role))
            .spawn(move || {
                let bridge = Bridge {
                    shared: thread_shared.clone(),
                    io: config.io,
                    generation: config.generation,
                    read: None,
                    window: None,
                    request_cancel: None,
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut instance = Instance::with_reservation(
                        InstanceConfig {
                            engine: config.engine,
                            module: config.module,
                            role: config.role,
                            owner: config.owner,
                            grants: config.grants,
                            ledger: config.ledger,
                            cancel: thread_shared.cancel.clone(),
                        },
                        Some(bridge),
                        reservation,
                    )?;
                    thread_shared.set_status(Status::Ready);
                    run(&mut instance, &thread_shared, &thread_updates)
                }))
                .unwrap_or(Err(ErrorCode::Internal));
                thread_shared.stop();
                thread_shared.set_status(match result {
                    Ok(()) | Err(ErrorCode::Cancelled) => Status::Stopped,
                    Err(error) => Status::Failed(error),
                });
                thread_shared.done.store(true, Ordering::Release);
                (thread_shared.wake)();
            })
            .map_err(|_| ErrorCode::Internal)?;
        Ok(Self { shared, updates })
    }

    pub fn send(&self, bytes: Vec<u8>) -> Result<(), ErrorCode> {
        self.shared.send(bytes, None)
    }
    pub fn open_window(&self, window: Id, generation: Generation) -> Result<(), ErrorCode> {
        self.shared.open_window(window, generation)
    }
    pub fn advance_window(&self, window: Id, generation: Generation) -> Result<(), ErrorCode> {
        self.shared.advance_window(window, generation)
    }
    pub fn close_window(&self, window: Id) -> Result<(), ErrorCode> {
        self.shared.close_window(window)
    }
    /// Bytes contain an unwrapped HostEvent. Only the host can select a window.
    pub fn send_window(
        &self,
        window: Id,
        generation: Generation,
        bytes: Vec<u8>,
    ) -> Result<(), ErrorCode> {
        self.shared.send_window(window, generation, bytes)
    }
    pub fn send_cancellable(
        &self,
        window: Id,
        generation: Generation,
        bytes: Vec<u8>,
        cancel: HttpCancel,
    ) -> Result<(), ErrorCode> {
        self.shared
            .send_cancellable(window, generation, bytes, Some(cancel))
    }
    /// Host-controlled attachment field, never a guest-selected destination.
    /// A fresh handle is reserved before queueing; cancellation drops it with
    /// the event. The original Blob bytes remain charged through all owners.
    pub fn send_context(
        &self,
        mut event: serde_json::Value,
        blob: Arc<Blob>,
    ) -> Result<(), ErrorCode> {
        if self.shared.role != Role::Stream {
            return Err(ErrorCode::Denied);
        }
        let object = event.as_object_mut().ok_or(ErrorCode::InvalidArgument)?;
        if !matches!(
            object.get("event").and_then(|v| v.as_str()),
            Some("stream_init" | "stream_context_changed")
        ) {
            return Err(ErrorCode::InvalidArgument);
        }
        let id = Id(next_id()?);
        let blob = DetachedBlob::reserve(&self.shared.ledger, &self.shared.owner, blob)?;
        object.insert("context_blob".into(), serde_json::json!(id));
        let bytes = crate::bounded_json::encode(&event, MESSAGE_LIMIT).map_err(json_error)?;
        self.shared.send(bytes, Some((id, blob)))
    }
    pub fn poll(&self) -> Option<Update> {
        let (lock, cv) = &*self.updates;
        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(update) = queue.items.pop_front() {
            cv.notify_one();
            if !self.shared.cancel.load(Ordering::Acquire)
                && self.shared.is_current(update.window, update.generation)
                && !update.is_cancelled()
            {
                return Some(update);
            }
        }
        None
    }
    pub fn refresh_context(
        &self,
        session: Id,
        expected: Generation,
        generation: Generation,
        blob: Arc<Blob>,
    ) -> Result<(), ErrorCode> {
        if self.shared.role != Role::Stream {
            return Err(ErrorCode::Denied);
        }
        let id = Id(next_id()?);
        let blob = DetachedBlob::reserve(&self.shared.ledger, &self.shared.owner, blob)?;
        let bytes = crate::bounded_json::encode(
            &serde_json::json!({"event":"stream_context_changed","session":session,"generation":generation,"context_blob":id}),
            MESSAGE_LIMIT,
        )
        .map_err(json_error)?;
        self.shared
            .refresh_context(expected, generation, bytes, (id, blob))
    }
    pub fn status(&self) -> Status {
        self.shared.status()
    }
    pub fn initialized(&self) -> bool {
        self.shared.initialized.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.shared.stop();
        let (lock, cv) = &*self.updates;
        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = true;
        queue.items.clear();
        cv.notify_all();
    }
    pub fn is_quiescent(&self) -> bool {
        if self.shared.cancel.load(Ordering::Acquire) {
            let (lock, cv) = &*self.updates;
            let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
            queue.items.clear();
            cv.notify_all();
        }
        self.shared.done.load(Ordering::Acquire)
            && self.shared.outstanding.load(Ordering::Acquire) == 0
            && self.shared.executor.is_quiescent()
    }
    pub fn advance(&self, generation: Generation) -> Result<(), ErrorCode> {
        self.shared.advance(generation)
    }

    pub fn read(
        &self,
        resource: String,
        offset: u64,
        len: u32,
        size: Option<u64>,
    ) -> Result<ReadTicket, ErrorCode> {
        self.shared.read(resource, offset, len, size)
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) struct TaskLease {
    _reservation: Reservation,
    _read: Option<Arc<mailbox::ReadLease>>,
    outstanding: Arc<AtomicUsize>,
    operation: u64,
    operations: Arc<Mutex<BTreeMap<u64, PendingOperation>>>,
}
#[derive(Clone)]
pub(super) struct PendingOperation {
    pub(super) cancel: HttpCancel,
    pub(super) window: Option<Id>,
}
impl Drop for TaskLease {
    fn drop(&mut self) {
        self.operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.operation);
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run(
    instance: &mut Instance,
    shared: &Arc<Mailbox>,
    updates: &SharedUpdates,
) -> Result<(), ErrorCode> {
    while let Some(mut message) = shared.next() {
        let (global, windows) = shared.contexts();
        instance.retain_contexts(global, &windows);
        if matches!(message, Message::Sweep) {
            continue;
        }
        if let Message::Cancelled { read, .. } = &message
            && !read.delivered
        {
            continue;
        }
        let _scratch = shared.ledger.reserve(
            &shared.owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
        )?;
        let generation = match &message {
            Message::Event { generation, .. } | Message::Completed { generation, .. } => {
                *generation
            }
            Message::Closed { .. } => global,
            Message::Cancelled { read, .. } => read.generation,
            Message::Sweep => unreachable!(),
        };
        let read = match &message {
            Message::Event { read, .. } | Message::Completed { read, .. } => *read,
            Message::Cancelled { .. } | Message::Closed { .. } => None,
            Message::Sweep => unreachable!(),
        };
        let window = message.window();
        let request_cancel = message.request_cancel();
        let is_closed = matches!(&message, Message::Closed { .. });
        let session_event =
            window.is_none() && matches!(&message, Message::Event { read: None, .. });
        // Terminal `Message::Closed` is only gated on actor cancellation, never global generation.
        let generation = if is_closed {
            if shared.cancel.load(Ordering::Acquire) {
                continue;
            }
            global
        } else if session_event {
            let Some(current) = shared.is_session_current(generation) else {
                continue;
            };
            current
        } else {
            if !shared.is_current(window, generation) {
                continue;
            }
            generation
        };
        if read.is_some_and(|id| !shared.has_read(id))
            || request_cancel
                .as_ref()
                .is_some_and(HttpCancel::is_cancelled)
        {
            continue;
        }
        instance.select_context(window, generation);
        instance.request_scope(request_cancel.clone());
        if let Message::Event { attachment, .. } = &mut message
            && let Some((id, blob)) = attachment.take()
        {
            instance.adopt_blob(id, blob)?;
        }
        if let Message::Completed {
            operation, result, ..
        } = &mut message
            && shared
                .operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(operation)
                .is_some_and(|pending| pending.cancel.is_cancelled())
        {
            *result = Err(ErrorCode::Cancelled);
        }
        let bytes = match &mut message {
            Message::Event { bytes, .. } => bytes.clone(),
            Message::Closed { window, .. } => {
                serde_json::to_vec(&serde_json::json!({"event":"window_closed","window":window}))
                    .map_err(|_| ErrorCode::Internal)?
            }
            Message::Cancelled { request, .. } => serde_json::to_vec(&serde_json::json!({
                "event":"stream_read_cancelled", "request_id":request, "generation":generation
            }))
            .map_err(|_| ErrorCode::Internal)?,
            Message::Sweep => unreachable!(),
            Message::Completed {
                operation,
                result,
                generation,
                ..
            } => {
                let result = match result {
                    Ok(outcome) => {
                        let mut value = outcome.value.clone();
                        let size = crate::bounded_json::encode(&value, MESSAGE_LIMIT - 1024)
                            .map_err(json_error);
                        let delivered = if let Err(error) = size {
                            Err(error)
                        } else if let Some(blob) = &outcome.blob {
                            match value.as_object_mut() {
                                Some(obj) => instance.deliver_blob(blob.clone()).map(|id| {
                                    obj.insert("blob".into(), serde_json::json!(Id(id)));
                                }),
                                None => Err(ErrorCode::InvalidArgument),
                            }
                        } else {
                            Ok(())
                        };
                        match delivered {
                            Ok(()) => {
                                outcome.acknowledge();
                                serde_json::json!({"ok":value})
                            }
                            Err(code) => serde_json::json!({"error":{"code":code as i32}}),
                        }
                    }
                    Err(error) => serde_json::json!({"error":{"code":*error as i32}}),
                };
                serde_json::to_vec(&serde_json::json!({"event":"completed","operation_id":Id(*operation),"generation":generation,"result":result}))
                    .map_err(|_| ErrorCode::Internal)?
            }
        };
        let bytes = if let Some(window) = window {
            let body: serde_json::Value =
                crate::bounded_json::decode(&bytes, MESSAGE_LIMIT).map_err(json_error)?;
            crate::bounded_json::encode(
                &serde_json::json!({"event":"window", "window":window,"generation":generation,"body":body}),
                MESSAGE_LIMIT,
            )
            .map_err(json_error)?
        } else {
            bytes
        };
        if let Message::Completed {
            result: Ok(outcome),
            ..
        } = &mut message
        {
            drop(outcome.pending.take());
        }
        if let Message::Completed { operation, .. } = &message {
            shared
                .operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(operation);
        }
        if let Some(id) = read
            && matches!(&message, Message::Event { .. })
            && !shared.deliver_read(id)
        {
            continue;
        }
        instance.begin_event(generation, read);
        let initialization = matches!(
            &message,
            Message::Event {
                initialization: true,
                ..
            }
        );
        let reply = if initialization {
            instance.call(&bytes, Duration::from_secs(5))?
        } else {
            instance.on_event(&bytes)?
        };
        match reply {
            Reply::Ok { ok } if !ok.is_empty() => {
                shared.validate_commands(&ok)?;
                let mut ids = BTreeSet::new();
                for id in ok.iter().flat_map(HostCommand::blobs) {
                    if !ids.insert(id) {
                        return Err(ErrorCode::InvalidArgument);
                    }
                }
                let blobs = instance.detach_blobs(&ids)?;
                if !shared.is_current(window, generation)
                    || request_cancel
                        .as_ref()
                        .is_some_and(HttpCancel::is_cancelled)
                {
                    continue;
                }
                let memory = shared.ledger.reserve(
                    &shared.owner,
                    &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
                )?;
                let update = Update {
                    commands: ok,
                    blobs,
                    generation,
                    window,
                    request_cancel,
                    _memory: memory,
                };
                if push_update(updates, shared, update, window, generation)? {
                    (shared.wake)();
                }
            }
            Reply::Error { error } => {
                return Err(ErrorCode::from_i32(error.code).unwrap_or(ErrorCode::Internal));
            }
            _ => {}
        }
        if initialization && !shared.initialized.swap(true, Ordering::AcqRel) {
            (shared.wake)();
        }
    }
    Ok(())
}

fn push_update(
    updates: &SharedUpdates,
    shared: &Arc<Mailbox>,
    update: Update,
    window: Option<Id>,
    generation: Generation,
) -> Result<bool, ErrorCode> {
    let (lock, cv) = &**updates;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut woke = false;
    let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
    while queue.items.len() >= 32 {
        if queue.closed || shared.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        if !shared.is_current(window, generation) || update.is_cancelled() {
            return Ok(false);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(ErrorCode::Backpressure);
        };
        if !woke {
            woke = true;
            drop(queue);
            (shared.wake)();
            queue = lock.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        queue = cv
            .wait_timeout(queue, remaining.min(Duration::from_millis(50)))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    if queue.closed || shared.cancel.load(Ordering::Acquire) {
        return Err(ErrorCode::Cancelled);
    }
    queue.items.push_back(update);
    Ok(true)
}

#[cfg(test)]
mod tests;
