//! Guest-created inert data artifacts use private, accounted temporary files.
//! The API accepts formats and bytes, never filesystem paths.
use super::{FileExecutor, Inner};
use crate::{
    plugin_v2::{
        actor::{IoOutcome, Operation, TaskContext},
        files::ArtifactWriter,
        instance::{Blob, next_id},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, Generation, Id, task::ArtifactFormat, task_result::ArtifactResult};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

pub(super) struct WriterEntry {
    writer: Option<ArtifactWriter>,
    pub cancel: HttpCancel,
    generation: Generation,
}

struct Work {
    writer: Option<ArtifactWriter>,
    inner: Weak<Inner>,
    id: Id,
    armed: bool,
}

impl Work {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if self.armed {
            discard(&self.inner, self.id);
        }
    }
}

fn discard(inner: &Weak<Inner>, id: Id) {
    if let Some(inner) = inner.upgrade()
        && let Some(entry) = inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writers
            .remove(&id)
    {
        entry.cancel.cancel();
    }
}

impl FileExecutor {
    pub(super) fn begin_artifact(
        &self,
        format: ArtifactFormat,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        let this = self.clone();
        let activity = self.0.activity.start();
        Ok(Operation::blocking(move || {
            let _activity = activity;
            this.bind(&context)?;
            let writer = this.0.root.writer(format.extension())?;
            let id = Id(next_id()?);
            let cancel = this.0.cancel.child().map_err(|_| ErrorCode::Cancelled)?;
            let mut state = this.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopped || context.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if state.generation != Some(context.generation) {
                return Err(ErrorCode::StaleHandle);
            }
            state.writers.insert(
                id,
                WriterEntry {
                    writer: Some(writer),
                    cancel,
                    generation: context.generation,
                },
            );
            drop(state);
            let weak = Arc::downgrade(&this.0);
            Ok(
                IoOutcome::new(serde_json::json!({"artifact_writer":id}), None)
                    .on_discard(move || discard(&weak, id)),
            )
        }))
    }

    pub(super) fn write_artifact(
        &self,
        id: Id,
        blob: Id,
        finish: bool,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        let data = blobs.get(&blob.0).cloned().ok_or(ErrorCode::StaleHandle)?;
        if data.owner != context.owner {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.writers.get_mut(&id).ok_or(ErrorCode::StaleHandle)?;
        if entry.generation != context.generation {
            return Err(ErrorCode::StaleHandle);
        }
        entry
            .cancel
            .link(&context.cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        let writer = entry.writer.take().ok_or(ErrorCode::Backpressure)?;
        drop(state);
        let this = self.clone();
        let activity = self.0.activity.start();
        let work = Work {
            writer: Some(writer),
            inner: Arc::downgrade(&self.0),
            id,
            armed: true,
        };
        Ok(Operation::blocking(move || {
            let mut work = work;
            let _activity = activity;
            this.bind(&context)?;
            work.writer
                .as_mut()
                .ok_or(ErrorCode::Internal)?
                .write(&data.bytes, &context.cancel)?;
            if finish {
                let artifact = work
                    .writer
                    .take()
                    .ok_or(ErrorCode::Internal)?
                    .finish(&context.cancel)?;
                let mut state = this.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.stopped || context.cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
                if state.generation != Some(context.generation)
                    || state.writers.remove(&id).is_none()
                {
                    return Err(ErrorCode::StaleHandle);
                }
                work.disarm();
                drop(state);
                let value = ArtifactResult {
                    artifact: artifact.id,
                    bytes: Id(artifact.size()),
                    format: artifact.format.clone(),
                };
                this.publish(artifact, value, &context)
            } else {
                let mut state = this.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.stopped || context.cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
                if state.generation != Some(context.generation) {
                    return Err(ErrorCode::StaleHandle);
                }
                let entry = state.writers.get_mut(&id).ok_or(ErrorCode::StaleHandle)?;
                entry.writer = work.writer.take();
                work.disarm();
                drop(state);
                let weak = Arc::downgrade(&this.0);
                Ok(IoOutcome::new(
                    serde_json::json!({"written":Id(data.bytes.len() as u64)}),
                    None,
                )
                .on_discard(move || discard(&weak, id)))
            }
        }))
    }
}
