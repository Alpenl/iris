//! Control-store services are composed once per installation and share underlying
//! storage, auth, and lazy artifact temp-root services across host windows.
use super::{
    actor::{Executor, Operation, TaskContext},
    auth::{AuthExecutor, BrowserOpen},
    engine::RuntimeEngine,
    files::{ArtifactCleaner, CleanerQueue, FileExecutor, FileSetup},
    instance::Blob,
    ledger::{Ledger, Reservation, Resource},
    network::{NetworkExecutor, NetworkGrant},
    pool::IoPool,
    storage::{Storage, StorageExecutor},
};
use iris_api::v2::{Capability, ErrorCode, Generation, Id, Role, task::Task};
use iris_core::vault::CredentialVault;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Resources {
    pub engine: RuntimeEngine,
    pub io: IoPool,
    pub ledger: Ledger,
    pub(crate) cleaner: ArtifactCleaner,
}

impl Resources {
    pub fn new() -> Result<Self, ErrorCode> {
        Ok(Self {
            engine: RuntimeEngine::new()?,
            io: IoPool::new()?,
            ledger: Ledger::default(),
            cleaner: ArtifactCleaner::new(),
        })
    }

    /// Drains pending artifact deletions within bounded retries/time and joins
    /// this host's cleanup worker within bounded timeout.
    pub fn shutdown(&self) {
        self.cleaner.shutdown();
    }
}

#[derive(Clone)]
pub struct Setup {
    pub resources: Resources,
    pub state_directory: PathBuf,
    pub temporary_directory: PathBuf,
    pub vault: CredentialVault,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}

struct WindowScope {
    generation: Generation,
    network: NetworkExecutor,
    files: FileExecutor,
    _lease: Reservation,
}

struct State {
    stopped: bool,
    active: Option<(Id, WindowScope)>,
    retired: Vec<WindowScope>,
}

pub(crate) struct ControlServices {
    owner: String,
    ledger: Ledger,
    network_grant: NetworkGrant,
    global_network: NetworkExecutor,
    global_files: FileExecutor,
    auth: AuthExecutor,
    storage: StorageExecutor,
    state: Mutex<State>,
}

impl ControlServices {
    pub(crate) fn new(
        owner: String,
        ledger: Ledger,
        network_grant: NetworkGrant,
        storage: Arc<Storage>,
        capabilities: BTreeSet<Capability>,
        temporary_directory: PathBuf,
        cleaner: CleanerQueue,
    ) -> Result<Self, ErrorCode> {
        let global_network =
            NetworkExecutor::new(network_grant.clone(), ledger.clone(), BTreeSet::new())?;
        let global_files = FileExecutor::with_cleaner(
            FileSetup {
                network: network_grant.clone(),
                ledger: ledger.clone(),
                capabilities: capabilities.clone(),
                temporary_directory,
            },
            cleaner,
        )?;
        let auth = AuthExecutor::new(
            network_grant.clone(),
            ledger.clone(),
            Arc::new(|_, _, _| Err(ErrorCode::Denied)),
        );
        let storage = StorageExecutor::new(storage, network_grant.clone(), capabilities)?;
        Ok(Self {
            owner,
            ledger,
            network_grant,
            global_network,
            global_files,
            auth,
            storage,
            state: Mutex::new(State {
                stopped: false,
                active: None,
                retired: Vec::new(),
            }),
        })
    }

    pub(crate) fn open_window(
        &self,
        window: Id,
        generation: Generation,
        browser: BrowserOpen,
    ) -> Result<FileExecutor, ErrorCode> {
        if window.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
        )?;
        let network = NetworkExecutor::new(
            self.network_grant.clone(),
            self.ledger.clone(),
            BTreeSet::new(),
        )?;
        let files = self.global_files.for_window();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::reap(&mut state);
        if state.stopped {
            return Err(ErrorCode::Cancelled);
        }
        if let Some((active_id, _)) = &state.active {
            return Err(if *active_id == window {
                ErrorCode::BadState
            } else {
                ErrorCode::Backpressure
            });
        }
        // Retired scopes continue to occupy capacity until all I/O exits.
        if state.retired.len() >= 4 {
            return Err(ErrorCode::Backpressure);
        }
        state.active = Some((
            window,
            WindowScope {
                generation,
                network,
                files: files.clone(),
                _lease: lease,
            },
        ));
        drop(state);
        self.auth.open_window(window, generation, browser);
        self.storage.window_changed(window, Some(generation));
        Ok(files)
    }

    fn reap(state: &mut State) {
        state
            .retired
            .retain(|s| !s.network.is_quiescent() || !s.files.is_quiescent());
    }

    fn window_executors(
        &self,
        window: Option<Id>,
        generation: Option<Generation>,
    ) -> Result<(NetworkExecutor, FileExecutor), ErrorCode> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return Err(ErrorCode::Cancelled);
        }
        match window {
            None => Ok((self.global_network.clone(), self.global_files.clone())),
            Some(id) => {
                let (_, scope) = state
                    .active
                    .as_ref()
                    .filter(|(active_id, _)| *active_id == id)
                    .ok_or(ErrorCode::StaleHandle)?;
                if generation.is_some_and(|g| g != scope.generation) {
                    return Err(ErrorCode::StaleHandle);
                }
                Ok((scope.network.clone(), scope.files.clone()))
            }
        }
    }
}

impl Executor for ControlServices {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if context.owner != self.owner || context.role != Role::Control {
            return Err(ErrorCode::Denied);
        }
        let (network, files) = self.window_executors(context.window, Some(context.generation))?;
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
            | Task::TcpWrite { .. } => network.prepare(task, blobs, context),
            Task::BeginAuth { .. } | Task::OpenAuth { .. } => {
                self.auth.prepare(task, blobs, context)
            }
            Task::HashCurrentMedia { .. }
            | Task::ArtifactBegin { .. }
            | Task::ArtifactWrite { .. }
            | Task::ArtifactRead { .. } => files.prepare(task, blobs, context),
        }
    }

    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        self.close_in(None, handle)
    }

    fn close_in(&self, window: Option<Id>, handle: u64) -> Result<(), ErrorCode> {
        let (network, files) = self.window_executors(window, None)?;
        match network.close(handle) {
            Err(ErrorCode::StaleHandle) => {}
            result => return result,
        }
        match self.auth.close_in(window, handle) {
            Err(ErrorCode::StaleHandle) => {}
            result => return result,
        }
        files.close(handle)
    }

    fn window_changed(&self, window: Id, generation: Option<Generation>) {
        let target = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopped {
                return;
            }
            if state.active.as_ref().is_none_or(|(id, _)| *id != window) {
                return;
            }
            if let Some(generation) = generation {
                let (_, scope) = state.active.as_mut().unwrap();
                scope.generation = generation;
                (scope.network.clone(), scope.files.clone())
            } else {
                let (_, scope) = state.active.take().unwrap();
                let target = (scope.network.clone(), scope.files.clone());
                state.retired.push(scope);
                target
            }
        };
        // Never run cancellation callbacks while holding the router lock.
        if let Some(generation) = generation {
            target.0.generation_changed(generation);
            target.1.generation_changed(generation);
        } else {
            target.0.stop();
            target.1.stop();
        }
        self.auth.window_changed(window, generation);
        self.storage.window_changed(window, generation);
    }

    fn generation_changed(&self, generation: Generation) {
        self.global_network.generation_changed(generation);
        self.global_files.generation_changed(generation);
        self.auth.generation_changed(generation);
        self.storage.generation_changed(generation);
    }

    fn stop(&self) {
        let scopes: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopped {
                return;
            }
            state.stopped = true;
            if let Some((_, active)) = state.active.take() {
                state.retired.push(active);
            }
            state
                .retired
                .iter()
                .map(|s| (s.network.clone(), s.files.clone()))
                .collect()
        };
        self.global_network.stop();
        self.global_files.stop();
        self.auth.stop();
        self.storage.stop();
        for (network, files) in scopes {
            network.stop();
            files.stop();
        }
    }

    fn is_quiescent(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::reap(&mut state);
        self.global_network.is_quiescent()
            && self.global_files.is_quiescent()
            && self.global_files.root_is_quiescent()
            && self.auth.is_quiescent()
            && self.storage.is_quiescent()
            && state
                .active
                .as_ref()
                .is_none_or(|(_, s)| s.network.is_quiescent() && s.files.is_quiescent())
            && state
                .retired
                .iter()
                .all(|s| s.network.is_quiescent() && s.files.is_quiescent())
    }
}

impl Drop for ControlServices {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
