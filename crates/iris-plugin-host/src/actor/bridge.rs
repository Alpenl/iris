use super::{
    PendingOperation, TaskContext, TaskLease,
    mailbox::{self, Mailbox, Message},
};
use crate::{
    instance::{Blob, next_id},
    ledger::Resource,
    pool::IoPool,
};
use iris_api::v2::{ErrorCode, Generation, Id, MESSAGE_LIMIT, task::Task};
use iris_net::http_client::HttpCancel;
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

pub(crate) struct Bridge {
    pub(super) shared: Arc<Mailbox>,
    pub(super) io: IoPool,
    pub(super) generation: Generation,
    pub(super) read: Option<Id>,
    pub(super) window: Option<Id>,
    pub(super) request_cancel: Option<HttpCancel>,
}

impl Bridge {
    pub(crate) fn on_request(&mut self, cancel: Option<HttpCancel>) {
        self.request_cancel = cancel;
    }

    pub(crate) fn on_window(&mut self, window: Option<Id>) {
        self.window = window;
    }

    pub(crate) fn on_event(&mut self, generation: Generation, read: Option<Id>) {
        self.generation = generation;
        self.read = read;
    }

    pub(crate) fn submit(
        &mut self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
    ) -> Result<u64, ErrorCode> {
        let shared = &self.shared;
        let mailbox::IoContext {
            generation,
            cancel,
            read: read_lease,
        } = shared.io_context(self.read, self.window)?;
        if let Some(parent) = &self.request_cancel {
            parent.link(&cancel).map_err(|_| ErrorCode::Cancelled)?;
        }
        if self.generation.media != generation.media
            || self.generation.seek != generation.seek
            || (self.window.is_some() && self.generation != generation)
        {
            return Err(ErrorCode::StaleHandle);
        }
        let reservation = shared.ledger.reserve(
            &shared.owner,
            &[
                (Resource::Task, 1),
                (Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64),
            ],
        )?;
        let context = TaskContext {
            instance_id: Id(shared.id),
            window: self.window,
            owner: shared.owner.clone(),
            role: shared.role,
            generation: self.generation,
            cancel: cancel.clone(),
            ledger: shared.ledger.clone(),
        };
        let operation = shared.executor.prepare(task, blobs, context)?;
        let id = next_id()?;
        shared.outstanding.fetch_add(1, Ordering::AcqRel);
        let lease = TaskLease {
            _reservation: reservation,
            _read: read_lease,
            outstanding: shared.outstanding.clone(),
            operation: id,
            operations: shared.operations.clone(),
        };
        shared
            .operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                PendingOperation {
                    cancel: cancel.clone(),
                    window: self.window,
                },
            );
        let weak = Arc::downgrade(shared);
        let generation = self.generation;
        let read = self.read;
        let window = self.window;
        let request_cancel = self.request_cancel.clone();
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        operation.dispatch(
            &self.io,
            shared.role,
            &shared.owner,
            Box::new(move |result| {
                let result = if cancel.is_cancelled() {
                    // Finish rollback before publishing the terminal event; a
                    // shadowed unused Result could otherwise outlive delivery.
                    drop(result);
                    Err(ErrorCode::Cancelled)
                } else {
                    result
                };
                if let Some(shared) = weak.upgrade() {
                    shared.complete(Message::Completed {
                        operation: id,
                        result,
                        generation,
                        read,
                        window,
                        request_cancel,
                        _lease: lease,
                    });
                }
            }),
        )?;
        Ok(id)
    }

    pub(crate) fn close(&mut self, handle: u64) -> Result<(), ErrorCode> {
        // Drop the operation-map lock before cancellation: the wake callback
        // can synchronously finish I/O and release its TaskLease.
        let pending = self
            .shared
            .operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .cloned();
        if let Some(pending) = pending {
            if pending.window != self.window || pending.cancel.is_cancelled() {
                return Err(ErrorCode::StaleHandle);
            }
            pending.cancel.cancel();
            return Ok(());
        }
        self.shared.executor.close_in(self.window, handle)
    }

    pub(crate) fn reply(
        &mut self,
        request: u64,
        blob: Arc<Blob>,
        eof: bool,
    ) -> Result<usize, ErrorCode> {
        self.shared.reply(request, blob, eof)
    }

    pub(crate) fn fail(&mut self, request: u64, error: ErrorCode) -> Result<(), ErrorCode> {
        self.shared.fail(request, error)
    }
}
