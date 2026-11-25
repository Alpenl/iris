//! Control-store services are composed once; no fallback retries can reinterpret
//! a denied task as a different privileged operation.
mod windows;
use super::{
    actor::{Executor, Operation, TaskContext},
    auth::AuthExecutor,
    files::FileExecutor,
    instance::Blob,
    network::NetworkExecutor,
    storage::StorageExecutor,
};
use iris_api::v2::{ErrorCode, Generation, task::Task};
use std::{collections::BTreeMap, sync::Arc};
pub use windows::WindowServices;
#[derive(Clone)]
pub struct HostServices {
    pub network: NetworkExecutor,
    pub auth: AuthExecutor,
    pub files: FileExecutor,
    pub storage: StorageExecutor,
}
impl Executor for HostServices {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        match &task {
            Task::StateRead { .. }
            | Task::StateWrite { .. }
            | Task::SecretRead { .. }
            | Task::SecretWrite { .. } => self.storage.prepare(task, blobs, context),
            Task::OriginLookup { .. }
            | Task::HttpStart { .. }
            | Task::HttpRead { .. }
            | Task::HttpWrite { .. }
            | Task::WsConnect { .. }
            | Task::WsRead { .. }
            | Task::WsWrite { .. }
            | Task::TcpConnect { .. }
            | Task::TcpRead { .. }
            | Task::TcpWrite { .. } => self.network.prepare(task, blobs, context),
            Task::BeginAuth { .. } | Task::OpenAuth { .. } => {
                self.auth.prepare(task, blobs, context)
            }
            Task::HashCurrentMedia { .. }
            | Task::ArtifactBegin { .. }
            | Task::ArtifactWrite { .. }
            | Task::ArtifactRead { .. } => self.files.prepare(task, blobs, context),
        }
    }
    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        for executor in [&self.network as &dyn Executor, &self.auth, &self.files] {
            match executor.close(handle) {
                Err(ErrorCode::StaleHandle) => {}
                result => return result,
            }
        }
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {
        self.network.stop();
        self.auth.stop();
        self.files.stop();
        self.storage.stop();
    }
    fn generation_changed(&self, generation: Generation) {
        self.network.generation_changed(generation);
        self.auth.generation_changed(generation);
        self.files.generation_changed(generation);
        self.storage.generation_changed(generation);
    }
    fn is_quiescent(&self) -> bool {
        self.network.is_quiescent()
            && self.auth.is_quiescent()
            && self.files.is_quiescent()
            && self.storage.is_quiescent()
    }
}
