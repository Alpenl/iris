mod run;
mod writer;
use super::binding::{ToolKind, TrustedModel, TrustedTool};
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        files::{Artifact, ExportGrant, FileGrants, TempRoot, hash_media, identity::PinnedFile},
        instance::{Blob, next_id},
        ledger::{Activity, Ledger, Resource},
        network::NetworkGrant,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    CHUNK_LIMIT, Capability, ErrorCode, Generation, Id, Role,
    task::{Task, ToolTask},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct ToolSetup {
    pub network: NetworkGrant,
    pub ledger: Ledger,
    pub capabilities: BTreeSet<Capability>,
    pub temporary_directory: PathBuf,
    pub tools: BTreeMap<ToolKind, Arc<TrustedTool>>,
    pub models: BTreeMap<Id, Arc<TrustedModel>>,
    pub catalog: super::catalog::ToolCatalog,
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
    exports: BTreeMap<Id, ExportGrant>,
    current: Option<(Id, Generation, CurrentFile)>,
}
#[derive(Clone)]
enum CurrentFile {
    Pinned(Arc<PinnedFile>),
    Path(PathBuf),
}
struct Inner {
    network: NetworkGrant,
    ledger: Ledger,
    capabilities: BTreeSet<Capability>,
    root: Arc<TempRoot>,
    tools: BTreeMap<ToolKind, Arc<TrustedTool>>,
    models: BTreeMap<Id, Arc<TrustedModel>>,
    catalog: super::catalog::ToolCatalog,
    files: FileGrants,
    state: Mutex<State>,
    activity: Activity,
    cancel: HttpCancel,
}
#[derive(Clone)]
pub struct ToolExecutor(Arc<Inner>);
impl ToolExecutor {
    /// Create on a host background worker; directory I/O is never done by an import.
    pub fn new(setup: ToolSetup) -> Result<Self, ErrorCode> {
        let root = TempRoot::new(
            &setup.temporary_directory,
            setup.network.owner().into(),
            setup.ledger.clone(),
        )?;
        Ok(Self(Arc::new(Inner {
            files: FileGrants::new(setup.network.owner().into(), setup.ledger.clone()),
            network: setup.network,
            ledger: setup.ledger,
            capabilities: setup.capabilities,
            root,
            tools: setup.tools,
            models: setup.models,
            catalog: setup.catalog,
            activity: Activity::default(),
            cancel: HttpCancel::default(),
            state: Mutex::new(State {
                stopped: false,
                instance: None,
                generation: None,
                artifacts: BTreeMap::new(),
                writers: BTreeMap::new(),
                exports: BTreeMap::new(),
                current: None,
            }),
        })))
    }
    pub fn input_grants(&self) -> &FileGrants {
        &self.0.files
    }
    /// The host save dialog creates this grant; guest requests alone never do.
    pub fn add_export(&self, grant: ExportGrant) -> Result<Id, ErrorCode> {
        if !self.0.capabilities.contains(&Capability::ExportFiles) {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return Err(ErrorCode::Cancelled);
        }
        let id = Id(next_id()?);
        state.exports.insert(id, grant);
        Ok(id)
    }
    /// Only the actual current local media selected by the host is admitted.
    pub fn set_current_media(
        &self,
        media: Id,
        generation: Generation,
        file: Arc<PinnedFile>,
    ) -> Result<(), ErrorCode> {
        if !self.0.capabilities.contains(&Capability::ReadState) {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || state.generation.is_some_and(|g| g != generation) {
            return Err(ErrorCode::StaleHandle);
        }
        state.current = Some((media, generation, CurrentFile::Pinned(file)));
        Ok(())
    }
    /// The native player supplies its current local path; it is never serialized
    /// to the guest. Opening, identity checks and hashing run on the I/O pool.
    pub fn set_current_path(
        &self,
        media: Id,
        generation: Generation,
        path: Option<PathBuf>,
    ) -> Result<(), ErrorCode> {
        if !self.0.capabilities.contains(&Capability::ReadState) {
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
    fn bind(&self, context: &TaskContext) -> Result<(), ErrorCode> {
        if context.role != Role::Control || context.owner != self.0.network.owner() {
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
    fn tool(&self, kind: ToolKind, cancel: &HttpCancel) -> Result<Arc<TrustedTool>, ErrorCode> {
        self.0
            .tools
            .get(&kind)
            .cloned()
            .filter(|tool| tool.kind == kind)
            .map(Ok)
            .unwrap_or_else(|| self.0.catalog.tool(kind, cancel))
    }
    fn artifact(&self, id: Id, generation: Generation) -> Result<Arc<Artifact>, ErrorCode> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.artifacts.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if entry.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(entry.artifact.clone())
    }
    pub fn approved_artifact(
        &self,
        owner: &str,
        id: Id,
        generation: Generation,
    ) -> Result<Arc<Artifact>, ErrorCode> {
        if owner != self.0.network.owner() {
            return Err(ErrorCode::Denied);
        }
        self.artifact(id, generation)
    }
    fn publish(
        &self,
        artifact: Arc<Artifact>,
        result: iris_api::v2::task_result::ArtifactResult,
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
impl Executor for ToolExecutor {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        self.bind(&context)?;
        if !task.authorized(&self.0.capabilities) {
            return Err(ErrorCode::Denied);
        }
        let this = self.clone();
        let activity = self.0.activity.start();
        match task {
            Task::QueryTools => {
                let (mut kinds, mut models) = self.0.catalog.availability();
                kinds.extend(self.0.tools.keys().copied());
                models.extend(self.0.models.keys().copied());
                kinds.sort();
                kinds.dedup();
                models.sort();
                models.dedup();
                let versions: Vec<_> = kinds
                    .iter()
                    .map(|kind| serde_json::json!({"kind":kind,"version":kind.supported_version()}))
                    .collect();
                let mut tasks = Vec::new();
                if kinds.contains(&ToolKind::Ffmpeg) {
                    tasks.push("extract_audio");
                    if kinds.contains(&ToolKind::Ffprobe) {
                        tasks.push("lossless_slice");
                    }
                }
                if kinds.contains(&ToolKind::WhisperCpp) && !models.is_empty() {
                    tasks.push("transcribe");
                }
                if kinds.contains(&ToolKind::YtDlp) {
                    tasks.push("resolve_metadata");
                }
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    this.bind(&context)?;
                    Ok(IoOutcome::new(
                        serde_json::json!({"tasks":tasks,"models":models,"tools":versions}),
                        None,
                    ))
                }))
            }
            Task::ArtifactBegin { format } => self.begin_artifact(format, context),
            Task::ArtifactWrite {
                handle,
                blob,
                finish,
            } => self.write_artifact(handle, blob, finish, blobs, context),
            Task::ExportArtifact { artifact, export } => {
                let artifact = self.artifact(artifact, context.generation)?;
                let export = self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exports
                    .remove(&export)
                    .ok_or(ErrorCode::StaleHandle)?;
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    this.bind(&context)?;
                    export.commit(
                        &artifact,
                        &context.owner,
                        context.generation,
                        &context.ledger,
                        &context.cancel,
                    )?;
                    Ok(IoOutcome::new(
                        serde_json::json!({"exported":true,"bytes":Id(artifact.size())}),
                        None,
                    ))
                }))
            }
            Task::HashCurrentMedia { media, algorithm } => {
                let file = {
                    let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                    let (id, generation, file) = state.current.as_ref().ok_or(ErrorCode::Denied)?;
                    if *id != media || *generation != context.generation {
                        return Err(ErrorCode::StaleHandle);
                    }
                    file.clone()
                };
                let lease = self.0.ledger.reserve(
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
            Task::InvokeTool { task } => {
                validate(&task)?;
                if matches!(task, ToolTask::LosslessSlice { .. })
                    && !self.0.capabilities.contains(&Capability::ExportFiles)
                {
                    return Err(ErrorCode::Denied);
                }
                let lease = self.0.ledger.reserve(
                    &context.owner,
                    &[(Resource::Tool, 1), (Resource::HostBytes, 32 << 20)],
                )?;
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    let _lease = lease;
                    this.bind(&context)?;
                    this.run(task, &context)
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
        if state.artifacts.remove(&Id(handle)).is_some()
            || state.exports.remove(&Id(handle)).is_some()
        {
            Ok(())
        } else {
            Err(ErrorCode::StaleHandle)
        }
    }
    fn stop(&self) {
        self.0.cancel.cancel();
        self.0.files.revoke();
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.artifacts.clear();
        for writer in state.writers.values() {
            writer.cancel.cancel();
        }
        state.writers.clear();
        state.exports.clear();
        state.current = None;
    }
    fn generation_changed(&self, generation: Generation) {
        self.0.files.revoke();
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = Some(generation);
        state.artifacts.clear();
        for writer in state.writers.values() {
            writer.cancel.cancel();
        }
        state.writers.clear();
        state.exports.clear();
        state.current = None;
    }
    fn is_quiescent(&self) -> bool {
        self.0.activity.is_idle() && self.0.root.is_quiescent()
    }
}
fn validate(task: &ToolTask) -> Result<(), ErrorCode> {
    match task {
        ToolTask::LosslessSlice {
            start_ms, end_ms, ..
        } if start_ms >= end_ms || end_ms.0 > 7 * 86400 * 1000 => Err(ErrorCode::InvalidArgument),
        ToolTask::ExtractAudio { track, .. } if *track > 63 => Err(ErrorCode::InvalidArgument),
        ToolTask::Transcribe {
            language: Some(language),
            ..
        } if language != "auto"
            && (!(language.len() == 2 || language.len() == 3)
                || !language.bytes().all(|b| b.is_ascii_lowercase())) =>
        {
            Err(ErrorCode::InvalidArgument)
        }
        ToolTask::ResolveMetadata { url, origin_grant } => {
            if origin_grant.is_some() {
                return Err(ErrorCode::Unsupported);
            }
            crate::plugin_v2::network::normalize_url(
                url,
                crate::plugin_v2::network::TransportKind::Http,
            )
            .map(|_| ())
        }
        _ => Ok(()),
    }
}
