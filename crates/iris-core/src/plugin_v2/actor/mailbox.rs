use super::{
    Blob, DetachedBlob, Executor, IoOutcome, Ledger, PendingOperation, ReadResult, ReadTicket,
    Reservation, Resource, Status, TaskLease, next_id,
};
use crate::vfs::http_client::HttpCancel;
use iris_api::v2::{
    CHUNK_LIMIT, Capability, ErrorCode, Generation, HostCommand, Id, MESSAGE_LIMIT, Role,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};
mod windows;

pub(super) enum Message {
    Sweep,
    Closed {
        window: Id,
        generation: Generation,
        _lease: Reservation,
    },
    Event {
        bytes: Vec<u8>,
        initialization: bool,
        request_cancel: Option<HttpCancel>,
        generation: Generation,
        window: Option<Id>,
        read: Option<Id>,
        attachment: Option<(Id, DetachedBlob)>,
        _memory: Reservation,
    },
    Completed {
        operation: u64,
        result: Result<IoOutcome, ErrorCode>,
        generation: Generation,
        window: Option<Id>,
        request_cancel: Option<HttpCancel>,
        read: Option<Id>,
        _lease: TaskLease,
    },
    Cancelled {
        request: Id,
        read: ReadPending,
    },
}
impl Message {
    pub fn request_cancel(&self) -> Option<HttpCancel> {
        match self {
            Self::Event { request_cancel, .. } | Self::Completed { request_cancel, .. } => {
                request_cancel.clone()
            }
            _ => None,
        }
    }
    pub fn window(&self) -> Option<Id> {
        match self {
            Self::Event { window, .. } | Self::Completed { window, .. } => *window,
            _ => None,
        }
    }
}
struct WindowContext {
    generation: Generation,
    cancel: HttpCancel,
    ready: bool,
    _lease: Reservation,
}
pub(super) struct ReadPending {
    pub offset: u64,
    pub len: u32,
    pub size: Option<u64>,
    pub generation: Generation,
    pub reply: mpsc::SyncSender<Result<ReadResult, ErrorCode>>,
    pub cancel: HttpCancel,
    pub lease: Arc<ReadLease>,
}
pub(super) struct ReadLease {
    _reservation: Reservation,
    slots: Arc<AtomicUsize>,
}
impl Drop for ReadLease {
    fn drop(&mut self) {
        self.slots.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) struct IoContext {
    pub generation: Generation,
    pub cancel: HttpCancel,
    pub read: Option<Arc<ReadLease>>,
}
struct State {
    status: Status,
    generation: Generation,
    io_cancel: HttpCancel,
    events: VecDeque<Message>,
    completions: VecDeque<Message>,
    reads: BTreeMap<u64, ReadPending>,
    prefer_completion: bool,
    windows: BTreeMap<Id, WindowContext>,
    sweep: bool,
    closed: VecDeque<Message>,
}
pub(super) struct Mailbox {
    pub id: u64,
    pub owner: String,
    pub role: Role,
    pub ledger: Ledger,
    pub executor: Arc<dyn Executor>,
    pub cancel: Arc<AtomicBool>,
    pub done: AtomicBool,
    pub initialized: AtomicBool,
    pub outstanding: Arc<AtomicUsize>,
    pub operations: Arc<Mutex<BTreeMap<u64, PendingOperation>>>,
    reads: Arc<AtomicUsize>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
    grants: BTreeSet<Capability>,
    state: Mutex<State>,
    changed: Condvar,
}

impl Mailbox {
    pub fn new(
        owner: String,
        role: Role,
        generation: Generation,
        grants: BTreeSet<Capability>,
        ledger: Ledger,
        executor: Arc<dyn Executor>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, ErrorCode> {
        Ok(Self {
            id: next_id()?,
            owner,
            role,
            ledger,
            executor,
            wake,
            grants,
            cancel: Arc::new(AtomicBool::new(false)),
            done: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            outstanding: Arc::new(AtomicUsize::new(0)),
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            reads: Arc::new(AtomicUsize::new(0)),
            changed: Condvar::new(),
            state: Mutex::new(State {
                status: Status::Initializing,
                generation,
                io_cancel: HttpCancel::default(),
                events: VecDeque::new(),
                completions: VecDeque::new(),
                reads: BTreeMap::new(),
                prefer_completion: true,
                windows: BTreeMap::new(),
                sweep: false,
                closed: VecDeque::new(),
            }),
        })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn status(&self) -> Status {
        self.lock().status
    }
    pub fn set_status(&self, status: Status) {
        self.lock().status = status;
        (self.wake)();
    }
    pub fn has_read(&self, id: Id) -> bool {
        self.lock().reads.contains_key(&id.0)
    }
    pub fn io_context(&self, read: Option<Id>, window: Option<Id>) -> Result<IoContext, ErrorCode> {
        let state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        if let Some(window) = window {
            if read.is_some() || self.role != Role::Control {
                return Err(ErrorCode::Denied);
            }
            let scope = state.windows.get(&window).ok_or(ErrorCode::StaleHandle)?;
            if !scope.ready {
                return Err(ErrorCode::Backpressure);
            }
            return Ok(IoContext {
                generation: scope.generation,
                cancel: scope.cancel.child().map_err(|_| ErrorCode::Cancelled)?,
                read: None,
            });
        }
        let (parent, read) = match read {
            Some(id) => {
                let read = state.reads.get(&id.0).ok_or(ErrorCode::Cancelled)?;
                (&read.cancel, Some(read.lease.clone()))
            }
            None => (&state.io_cancel, None),
        };
        Ok(IoContext {
            generation: state.generation,
            cancel: parent.child().map_err(|_| ErrorCode::Cancelled)?,
            read,
        })
    }

    pub fn send(
        &self,
        bytes: Vec<u8>,
        attachment: Option<(Id, DetachedBlob)>,
    ) -> Result<(), ErrorCode> {
        let memory = self.ledger.reserve(
            &self.owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
        )?;
        let value: serde_json::Value = crate::plugin_v2::json::decode(&bytes, MESSAGE_LIMIT)?;
        let initialization = value.get("event").and_then(|v| v.as_str()) == Some("init");
        let mut state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        if state.events.len() >= 32 {
            return Err(ErrorCode::Backpressure);
        }
        let generation = state.generation;
        state.events.push_back(Message::Event {
            bytes,
            initialization,
            request_cancel: None,
            generation,
            window: None,
            read: None,
            attachment,
            _memory: memory,
        });
        self.changed.notify_one();
        Ok(())
    }
    pub fn refresh_context(
        &self,
        expected: Generation,
        generation: Generation,
        bytes: Vec<u8>,
        attachment: (Id, DetachedBlob),
    ) -> Result<(), ErrorCode> {
        let memory = self.ledger.reserve(
            &self.owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
        )?;
        let mut state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        if state.generation.media != expected.media
            || expected.media != generation.media
            || generation.context.0 <= expected.context.0
            || generation.context.0 <= state.generation.context.0
        {
            return Err(ErrorCode::StaleHandle);
        }
        let mut updated = state.generation;
        updated.context = generation.context;
        let generation = updated;
        if state.events.len() >= 32 {
            return Err(ErrorCode::Backpressure);
        }
        // Commit queue admission and version together. A concurrent read cannot
        // observe the new version before the context event is queued.
        state.events.push_back(Message::Event {
            bytes,
            initialization: false,
            request_cancel: None,
            generation,
            window: None,
            read: None,
            attachment: Some(attachment),
            _memory: memory,
        });
        state.generation = generation;
        self.changed.notify_one();
        Ok(())
    }
    pub fn next(&self) -> Option<Message> {
        let mut state = self.lock();
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return None;
            }
            if std::mem::take(&mut state.sweep) {
                return Some(Message::Sweep);
            }
            if let Some(message) = state.closed.pop_front() {
                return Some(message);
            }
            let message = if state.prefer_completion {
                state
                    .completions
                    .pop_front()
                    .or_else(|| state.events.pop_front())
            } else {
                state
                    .events
                    .pop_front()
                    .or_else(|| state.completions.pop_front())
            };
            state.prefer_completion = !state.prefer_completion;
            if message.is_some() {
                return message;
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
    pub fn complete(&self, message: Message) {
        let mut state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return;
        }
        // Every accepted task already owns one of 32 per-plugin task slots;
        // ordinary events cannot occupy its terminal delivery capacity.
        state.completions.push_back(message);
        self.changed.notify_one();
    }
    pub fn advance(&self, generation: Generation) -> Result<(), ErrorCode> {
        let (cancel, reads) = {
            let mut state = self.lock();
            if self.cancel.load(Ordering::Acquire) {
                return Err(ErrorCode::Cancelled);
            }
            let old = state.generation;
            if generation.media.0 < old.media.0
                || generation.seek.0 < old.seek.0
                || generation.context.0 < old.context.0
                || generation == old
            {
                return Err(ErrorCode::StaleHandle);
            }
            state.generation = generation;
            if generation.media == old.media && generation.seek == old.seek {
                // Context refresh alone permits in-flight reads bound to the old
                // version. Explicit revocation must use stop/seek cancellation.
                return Ok(());
            }
            state
                .events
                .retain(|m| m.window().is_some() || matches!(m, Message::Event { read: None, .. }));
            state.completions.retain(|m| m.window().is_some());
            state.sweep = true;
            (
                std::mem::take(&mut state.io_cancel),
                std::mem::take(&mut state.reads),
            )
        };
        cancel.cancel();
        self.executor.generation_changed(generation);
        for (_, read) in reads {
            let _ = read.reply.try_send(Err(ErrorCode::Cancelled));
        }
        self.changed.notify_one();
        Ok(())
    }
    pub fn stop(&self) {
        if self.cancel.swap(true, Ordering::AcqRel) {
            return;
        }
        let (cancel, reads, windows) = {
            let mut state = self.lock();
            state.status = Status::Stopping;
            state.events.clear();
            state.completions.clear();
            state.closed.clear();
            (
                std::mem::take(&mut state.io_cancel),
                std::mem::take(&mut state.reads),
                std::mem::take(&mut state.windows),
            )
        };
        cancel.cancel();
        for (_, scope) in windows {
            scope.cancel.cancel();
        }
        for (_, read) in reads {
            let _ = read.reply.try_send(Err(ErrorCode::Cancelled));
        }
        self.executor.stop();
        self.changed.notify_one();
        (self.wake)();
    }
    pub fn read(
        self: &Arc<Self>,
        resource: String,
        offset: u64,
        len: u32,
        size: Option<u64>,
    ) -> Result<ReadTicket, ErrorCode> {
        if self.role != Role::Stream {
            return Err(ErrorCode::Denied);
        }
        if len == 0
            || len as usize > CHUNK_LIMIT
            || resource.is_empty()
            || resource.len() > 512
            || offset.checked_add(u64::from(len)).is_none()
            || size.is_some_and(|n| offset > n)
        {
            return Err(ErrorCode::RangeInvalid);
        }
        let reservation = self.ledger.reserve(
            &self.owner,
            &[(Resource::StreamRead, 1), (Resource::HostBytes, 4096)],
        )?;
        self.reads
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| ErrorCode::Backpressure)?;
        let lease = Arc::new(ReadLease {
            _reservation: reservation,
            slots: self.reads.clone(),
        });
        let memory = self.ledger.reserve(
            &self.owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
        )?;
        let mut state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        if state.reads.len() >= 4 || state.events.len() >= 32 {
            return Err(ErrorCode::Backpressure);
        }
        let request = next_id()?;
        let generation = state.generation;
        let bytes = serde_json::to_vec(
            &serde_json::json!({"event":"stream_read","request_id":Id(request),
            "generation":generation,"resource_id":resource,"offset":Id(offset),"len":len}),
        )
        .map_err(|_| ErrorCode::Internal)?;
        let (tx, rx) = mpsc::sync_channel(1);
        let cancel = state.io_cancel.child().map_err(|_| ErrorCode::Cancelled)?;
        state.reads.insert(
            request,
            ReadPending {
                offset,
                len,
                size,
                generation,
                reply: tx,
                cancel,
                lease,
            },
        );
        state.events.push_back(Message::Event {
            bytes,
            initialization: false,
            request_cancel: None,
            generation,
            read: Some(Id(request)),
            window: None,
            attachment: None,
            _memory: memory,
        });
        self.changed.notify_one();
        Ok(ReadTicket {
            request,
            shared: self.clone(),
            rx,
        })
    }
    pub fn reply(&self, request: u64, blob: Arc<Blob>, eof: bool) -> Result<usize, ErrorCode> {
        let mut state = self.lock();
        let read = state.reads.get(&request).ok_or(ErrorCode::StaleHandle)?;
        let count = blob.bytes.len();
        if read.generation.media != state.generation.media
            || read.generation.seek != state.generation.seek
        {
            return Err(ErrorCode::StaleHandle);
        }
        if count > read.len as usize || (count == 0 && !eof) {
            return Err(ErrorCode::RangeInvalid);
        }
        let end = read
            .offset
            .checked_add(count as u64)
            .ok_or(ErrorCode::RangeInvalid)?;
        if read
            .size
            .is_some_and(|size| end > size || (eof && end != size))
        {
            return Err(ErrorCode::RangeInvalid);
        }
        let read = state.reads.remove(&request).ok_or(ErrorCode::StaleHandle)?;
        drop(state);
        read.cancel.cancel();
        read.reply
            .try_send(Ok(ReadResult { blob, eof }))
            .map_err(|_| ErrorCode::Cancelled)?;
        Ok(count)
    }
    pub fn fail(&self, request: u64, error: ErrorCode) -> Result<(), ErrorCode> {
        let read = self
            .lock()
            .reads
            .remove(&request)
            .ok_or(ErrorCode::StaleHandle)?;
        read.cancel.cancel();
        read.reply
            .try_send(Err(error))
            .map_err(|_| ErrorCode::Cancelled)
    }
    pub fn cancel_read(&self, request: u64) {
        let read = self.lock().reads.remove(&request);
        if let Some(read) = read {
            read.cancel.cancel();
            let _ = read.reply.try_send(Err(ErrorCode::Cancelled));
            // Cancellation delivery keeps the read reservation. A guest that
            // never drains it cannot create an unbounded notification queue.
            self.complete(Message::Cancelled {
                request: Id(request),
                read,
            });
        }
    }

    pub fn validate_commands(&self, commands: &[HostCommand]) -> Result<(), ErrorCode> {
        if self.role != Role::Control {
            return Err(ErrorCode::Denied);
        }
        for command in commands {
            if !command.authorized(&self.grants) {
                return Err(ErrorCode::Denied);
            }
            crate::plugin_v2::validation::command(command)?;
        }
        Ok(())
    }
}
