use super::mailbox::Mailbox;
use crate::{plugin_v2::instance::Blob, vfs::http_client::HttpCancel};
use iris_api::v2::ErrorCode;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

pub struct ReadResult {
    pub blob: Arc<Blob>,
    pub eof: bool,
}

pub(super) struct ReadSlot {
    value: Mutex<Option<Result<ReadResult, ErrorCode>>>,
    ready: Condvar,
}

impl ReadSlot {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(None),
            ready: Condvar::new(),
        })
    }

    pub(super) fn deliver(&self, result: Result<ReadResult, ErrorCode>) -> bool {
        let mut slot = self.value.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(result);
        self.ready.notify_all();
        true
    }
}

pub struct ReadTicket {
    pub(super) request: u64,
    shared: Arc<Mailbox>,
    slot: Arc<ReadSlot>,
}

impl ReadTicket {
    pub(super) fn new(request: u64, shared: Arc<Mailbox>, slot: Arc<ReadSlot>) -> Self {
        Self {
            request,
            shared,
            slot,
        }
    }

    /// PlaybackRelay calls this on its worker, never while holding a Store.
    pub fn receive(&self, timeout: Duration) -> Result<ReadResult, ErrorCode> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.slot.value.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ErrorCode::Timeout);
            };
            guard = self
                .slot
                .ready
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Immediate Condvar wakeup on either reply delivery or `cancel`.
    pub fn receive_cancellable(
        &self,
        cancel: &HttpCancel,
        timeout: Duration,
    ) -> Result<ReadResult, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let wake_slot = self.slot.clone();
        let _reg = cancel
            .register_callback(Arc::new(move || {
                let _guard = wake_slot.value.lock().unwrap_or_else(|e| e.into_inner());
                wake_slot.ready.notify_all();
            }))
            .map_err(|_| ErrorCode::Cancelled)?;
        let deadline = Instant::now() + timeout;
        let mut guard = self.slot.value.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            if cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ErrorCode::Timeout);
            };
            guard = self
                .slot
                .ready
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for ReadTicket {
    fn drop(&mut self) {
        self.shared.cancel_read(self.request);
    }
}
