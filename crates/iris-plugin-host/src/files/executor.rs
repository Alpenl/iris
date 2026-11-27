//! Scoped inert artifact staging and current-media hashing executor.
mod writer;
use super::{Artifact, ArtifactCleaner, CleanerQueue, TempRoot, hash_media, identity::PinnedFile};
use crate::{
    actor::{Executor, IoOutcome, Operation, TaskContext},
    instance::Blob,
    ledger::{Activity, Ledger, Resource},
    network::NetworkGrant,
};
use iris_api::v2::{
    CHUNK_LIMIT, Capability, ErrorCode, Generation, Id, Role, task::Task,
    task_result::ArtifactResult,
};
use iris_base::net::http_client::HttpCancel;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(crate) struct FileSetup {
    pub network: NetworkGrant,
    pub ledger: Ledger,
    pub capabilities: BTreeSet<Capability>,
    pub temporary_directory: PathBuf,
}

struct ArtifactEntry {
    generation: Generation,
    artifact: Arc<Artifact>,
}

struct State {
    stopped: bool,
    instance: Option<Id>,
    generation: Option<Generation>,
    artifacts: BTreeMap<Id, ArtifactEntry>,
    writers: BTreeMap<Id, writer::WriterEntry>,
    current: Option<(Id, Generation, CurrentFile)>,
}

#[derive(Clone)]
enum CurrentFile {
    Pinned(Arc<PinnedFile>),
    Path(PathBuf),
}

struct Shared {
    network: NetworkGrant,
    ledger: Ledger,
    capabilities: BTreeSet<Capability>,
    temp_dir: PathBuf,
    _standalone_cleaner: Option<ArtifactCleaner>,
    cleaner: CleanerQueue,
    root: Mutex<Option<Arc<TempRoot>>>,
}

struct Inner {
    shared: Arc<Shared>,
    state: Mutex<State>,
    activity: Activity,
    cancel: HttpCancel,
}

#[derive(Clone)]
pub(crate) struct FileExecutor(Arc<Inner>);

impl FileExecutor {
    pub(crate) fn with_cleaner(setup: FileSetup, cleaner: CleanerQueue) -> Result<Self, ErrorCode> {
        Ok(Self::from_shared(Arc::new(Shared {
            network: setup.network,
            ledger: setup.ledger,
            capabilities: setup.capabilities,
            temp_dir: setup.temporary_directory,
            _standalone_cleaner: None,
            cleaner,
            root: Mutex::new(None),
        })))
    }

    fn from_shared(shared: Arc<Shared>) -> Self {
        Self(Arc::new(Inner {
            shared,
            activity: Activity::default(),
            cancel: HttpCancel::default(),
            state: Mutex::new(State {
                stopped: false,
                instance: None,
                generation: None,
                artifacts: BTreeMap::new(),
                writers: BTreeMap::new(),
                current: None,
            }),
        }))
    }

    /// Creates an isolated window-scoped executor sharing the installation's lazy `TempRoot`.
    pub(crate) fn for_window(&self) -> Self {
        Self::from_shared(self.0.shared.clone())
    }

    pub(crate) fn root_is_quiescent(&self) -> bool {
        self.0
            .shared
            .root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|root| root.is_quiescent())
    }

    pub(super) fn temp_root(&self) -> Result<Arc<TempRoot>, ErrorCode> {
        let mut slot = self.0.shared.root.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        let created = TempRoot::with_cleaner(
            &self.0.shared.temp_dir,
            self.0.shared.network.owner().into(),
            self.0.shared.ledger.clone(),
            self.0.shared.cleaner.clone(),
        )?;
        *slot = Some(created.clone());
        Ok(created)
    }

    /// The native player supplies its current local path; it is never serialized
    /// to the guest. Opening, identity checks and hashing run on the I/O pool.
    pub(crate) fn set_current_path(
        &self,
        media: Id,
        generation: Generation,
        path: Option<PathBuf>,
    ) -> Result<(), ErrorCode> {
        if !self.0.shared.capabilities.contains(&Capability::ReadState) {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || state.generation.is_some_and(|g| g != generation) {
            return Err(ErrorCode::StaleHandle);
        }
        if path.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err(ErrorCode::Denied);
        }
        state.current = path.map(|p| (media, generation, CurrentFile::Path(p)));
        Ok(())
    }

    pub(super) fn bind(&self, context: &TaskContext) -> Result<(), ErrorCode> {
        if context.role != Role::Control || context.owner != self.0.shared.network.owner() {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || context.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if state.instance.is_some_and(|id| id != context.instance_id) {
            return Err(ErrorCode::Denied);
        }
        if state.generation.is_some_and(|g| g != context.generation) {
            return Err(ErrorCode::StaleHandle);
        }
        state.instance = Some(context.instance_id);
        state.generation.get_or_insert(context.generation);
        self.0
            .cancel
            .link(&context.cancel)
            .map_err(|_| ErrorCode::Cancelled)
    }

    fn artifact(&self, id: Id, generation: Generation) -> Result<Arc<Artifact>, ErrorCode> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.artifacts.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if entry.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(entry.artifact.clone())
    }

    pub(crate) fn approved_artifact(
        &self,
        owner: &str,
        id: Id,
        generation: Generation,
    ) -> Result<Arc<Artifact>, ErrorCode> {
        if owner != self.0.shared.network.owner() {
            return Err(ErrorCode::Denied);
        }
        self.artifact(id, generation)
    }

    pub(super) fn publish(
        &self,
        artifact: Arc<Artifact>,
        result: ArtifactResult,
        context: &TaskContext,
    ) -> Result<IoOutcome, ErrorCode> {
        self.bind(context)?;
        let value = serde_json::to_value(result).map_err(|_| ErrorCode::Internal)?;
        let id = artifact.id;
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || context.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if state.generation != Some(context.generation) {
            return Err(ErrorCode::StaleHandle);
        }
        state.artifacts.insert(
            id,
            ArtifactEntry {
                generation: context.generation,
                artifact,
            },
        );
        drop(state);
        let weak = Arc::downgrade(&self.0);
        Ok(IoOutcome::new(value, None).on_discard(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .artifacts
                    .remove(&id);
            }
        }))
    }
}

impl Executor for FileExecutor {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        self.bind(&context)?;
        if !task.authorized(&self.0.shared.capabilities) {
            return Err(ErrorCode::Denied);
        }
        let this = self.clone();
        let activity = self.0.activity.start();
        match task {
            Task::ArtifactBegin { format } => self.begin_artifact(format, context),
            Task::ArtifactWrite {
                handle,
                blob,
                finish,
            } => self.write_artifact(handle, blob, finish, blobs, context),
            Task::HashCurrentMedia { media, algorithm } => {
                let file = {
                    let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                    let (id, generation, file) = state.current.as_ref().ok_or(ErrorCode::Denied)?;
                    if *id != media || *generation != context.generation {
                        return Err(ErrorCode::StaleHandle);
                    }
                    file.clone()
                };
                let lease = self.0.shared.ledger.reserve(
                    &context.owner,
                    &[(Resource::FileHash, 1), (Resource::HostBytes, 65536)],
                )?;
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    let _lease = lease;
                    this.bind(&context)?;
                    let file = match file {
                        CurrentFile::Pinned(file) => file,
                        CurrentFile::Path(path) => {
                            let pinned = Arc::new(PinnedFile::open(&path)?);
                            let mut state = this.0.state.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some((cur_id, cur_gen, cur_file)) = state.current.as_mut()
                                && *cur_id == media
                                && *cur_gen == context.generation
                            {
                                *cur_file = CurrentFile::Pinned(pinned.clone());
                            }
                            pinned
                        }
                    };
                    let hash = hash_media(&file, algorithm, &context.cancel, |_, _| {})?;
                    this.bind(&context)?;
                    Ok(IoOutcome::new(
                        serde_json::to_value(hash).map_err(|_| ErrorCode::Internal)?,
                        None,
                    ))
                }))
            }
            Task::ArtifactRead {
                handle,
                offset,
                len,
            } => {
                if len == 0 || len as usize > CHUNK_LIMIT {
                    return Err(ErrorCode::RangeInvalid);
                }
                let artifact = self.artifact(handle, context.generation)?;
                let count = (len as u64).min(
                    artifact
                        .size()
                        .checked_sub(offset.0)
                        .ok_or(ErrorCode::RangeInvalid)?,
                ) as usize;
                let lease = self
                    .0
                    .shared
                    .ledger
                    .reserve(&context.owner, &[(Resource::HostBytes, count as u64)])?;
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    let _lease = lease;
                    this.bind(&context)?;
                    let mut bytes = vec![0; count];
                    let n = artifact.read_at(offset.0, &mut bytes, &context.cancel)?;
                    bytes.truncate(n);
                    let blob = Arc::new(Blob::new(&context.ledger, &context.owner, bytes)?);
                    Ok(IoOutcome::new(
                        serde_json::json!({"eof":offset.0+n as u64==artifact.size()}),
                        Some(blob),
                    ))
                }))
            }
            _ => Err(ErrorCode::Unsupported),
        }
    }

    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(writer) = state.writers.remove(&Id(handle)) {
            writer.cancel.cancel();
            return Ok(());
        }
        if state.artifacts.remove(&Id(handle)).is_some() {
            Ok(())
        } else {
            Err(ErrorCode::StaleHandle)
        }
    }

    fn stop(&self) {
        self.0.cancel.cancel();
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.artifacts.clear();
        for writer in state.writers.values() {
            writer.cancel.cancel();
        }
        state.writers.clear();
        state.current = None;
    }

    fn generation_changed(&self, generation: Generation) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = Some(generation);
        state.artifacts.clear();
        for writer in state.writers.values() {
            writer.cancel.cancel();
        }
        state.writers.clear();
        state.current = None;
    }

    fn is_quiescent(&self) -> bool {
        self.0.activity.is_idle()
    }
}

#[cfg(test)]
mod tests {
    use super::{ArtifactCleaner, ErrorCode, FileExecutor, FileSetup, Mutex, Shared};
    use std::sync::Arc;

    impl FileExecutor {
        pub(crate) fn new(setup: FileSetup) -> Result<Self, ErrorCode> {
            let cleaner = ArtifactCleaner::new();
            let queue = cleaner.queue();
            Ok(Self::from_shared(Arc::new(Shared {
                network: setup.network,
                ledger: setup.ledger,
                capabilities: setup.capabilities,
                temp_dir: setup.temporary_directory,
                _standalone_cleaner: Some(cleaner),
                cleaner: queue,
                root: Mutex::new(None),
            })))
        }
    }
}
