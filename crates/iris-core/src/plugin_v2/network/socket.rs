mod protocol;
mod pump;
use super::RequestGrant;
use crate::{
    plugin_v2::{
        actor::{Completion, IoOutcome, Operation, TaskContext},
        instance::Blob,
        ledger::Reservation,
    },
    vfs::{HttpCancel, http_client::duplex},
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
use mio::{Poll, Token, Waker};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use url::Url;

pub(super) enum Action {
    Read(u32),
    Write { blob: Arc<Blob>, text: bool },
}
pub(super) struct Pending {
    action: Action,
    context: TaskContext,
    done: Completion,
    deadline: Instant,
}
struct Queue {
    requests: VecDeque<Pending>,
    closed: Option<ErrorCode>,
}
struct Inner {
    queue: Mutex<Queue>,
    wake: Arc<Waker>,
    cancel: HttpCancel,
    read_busy: AtomicBool,
    write_busy: AtomicBool,
    ws: bool,
    _reservation: Reservation,
}
struct Busy {
    inner: Arc<Inner>,
    write: bool,
}
impl Drop for Busy {
    fn drop(&mut self) {
        let slot = if self.write {
            &self.inner.write_busy
        } else {
            &self.inner.read_busy
        };
        slot.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub(super) struct Socket(Arc<Inner>);
impl Socket {
    /// Called on a bounded I/O worker. Only the subsequent read/write registration
    /// runs on a Store thread; connection and protocol handshakes never do.
    pub(super) fn connect(
        url: &Url,
        headers: &[(String, String)],
        ws: bool,
        grant: Arc<RequestGrant>,
        cancel: HttpCancel,
        reservation: Reservation,
    ) -> Result<Self, ErrorCode> {
        let wire = duplex::Wire::connect(url, grant.clone(), &cancel)?;
        let mut protocol = protocol::Protocol::new(wire, url, headers, ws)?;
        protocol
            .wire()
            .nonblocking()
            .map_err(|_| ErrorCode::IoError)?;
        grant.check_live()?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let poll = Poll::new().map_err(|_| ErrorCode::IoError)?;
        let wake = Arc::new(Waker::new(poll.registry(), Token(1)).map_err(|_| ErrorCode::IoError)?);
        let cancel_wake = duplex::cancellation_wake(&cancel, wake.clone())?;
        let inner = Arc::new(Inner {
            queue: Mutex::new(Queue {
                requests: VecDeque::new(),
                closed: None,
            }),
            wake,
            cancel,
            read_busy: AtomicBool::new(false),
            write_busy: AtomicBool::new(false),
            ws,
            _reservation: reservation,
        });
        let worker = inner.clone();
        let activity = grant.background();
        std::thread::Builder::new()
            .name("iris-v2-socket".into())
            .spawn(move || {
                let _activity = activity;
                let _cancel_wake = cancel_wake;
                pump::run(protocol, poll, worker, grant);
            })
            .map_err(|_| ErrorCode::Internal)?;
        Ok(Self(inner))
    }
    pub(super) fn operation(
        &self,
        action: Action,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if context.cancel.is_cancelled() || self.0.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let write = matches!(action, Action::Write { .. });
        match &action {
            Action::Read(len) if *len == 0 || *len as usize > CHUNK_LIMIT => {
                return Err(ErrorCode::InvalidArgument);
            }
            Action::Write { blob, text }
                if blob.bytes.len() > CHUNK_LIMIT
                    || (*text && std::str::from_utf8(&blob.bytes).is_err()) =>
            {
                return Err(ErrorCode::InvalidArgument);
            }
            _ => {}
        }
        let slot = if write {
            &self.0.write_busy
        } else {
            &self.0.read_busy
        };
        slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ErrorCode::BadState)?;
        let busy = Busy {
            inner: self.0.clone(),
            write,
        };
        context
            .cancel
            .link(&self.0.cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        let inner = self.0.clone();
        Ok(Operation::deferred(move |done| {
            let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(error) = queue.closed {
                return Err(error);
            }
            if inner.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if queue.requests.len() >= 2 {
                return Err(ErrorCode::Backpressure);
            }
            queue.requests.push_back(Pending {
                action,
                context,
                deadline: Instant::now() + Duration::from_secs(30),
                done: Box::new(move |result| match result {
                    Ok(mut outcome) => {
                        outcome.pending = Some(Box::new(busy));
                        done(Ok(outcome));
                    }
                    Err(error) => {
                        drop(busy);
                        done(Err(error));
                    }
                }),
            });
            let _ = inner.wake.wake();
            Ok(())
        }))
    }
    pub(super) fn is_ws(&self) -> bool {
        self.0.ws
    }
    pub(super) fn close(&self) {
        self.0.cancel.cancel();
        let _ = self.0.wake.wake();
    }
}

impl Pending {
    fn finish(self, result: Result<IoOutcome, ErrorCode>) {
        (self.done)(result);
    }
}
