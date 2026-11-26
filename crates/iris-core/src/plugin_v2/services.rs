//! Control-store services are composed once per installation and share underlying
//! storage, auth, and lazy artifact temp-root services across host windows.
use super::{
    actor::{Executor, Operation, TaskContext},
    auth::{AuthExecutor, BrowserOpen},
    files::{FileExecutor, FileSetup},
    instance::Blob,
    ledger::{Ledger, Reservation, Resource},
    network::{NetworkExecutor, NetworkGrant},
    storage::{Storage, StorageExecutor},
};
use iris_api::v2::{Capability, ErrorCode, Generation, Id, Role, task::Task};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct WindowScope {
    generation: Generation,
    network: NetworkExecutor,
    files: FileExecutor,
    closed: bool,
    _lease: Reservation,
}

struct State {
    stopped: bool,
    scopes: BTreeMap<Id, WindowScope>,
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
    ) -> Result<Self, ErrorCode> {
        let global_network =
            NetworkExecutor::new(network_grant.clone(), ledger.clone(), BTreeSet::new())?;
        let global_files = FileExecutor::new(FileSetup {
            network: network_grant.clone(),
            ledger: ledger.clone(),
            capabilities: capabilities.clone(),
            temporary_directory,
        })?;
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
                scopes: BTreeMap::new(),
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
        if state.scopes.contains_key(&window) {
            return Err(ErrorCode::BadState);
        }
        // Retired scopes continue to occupy capacity until all I/O exits.
        if state.scopes.len() >= 8 || state.scopes.values().filter(|s| !s.closed).count() >= 4 {
            return Err(ErrorCode::Backpressure);
        }
        state.scopes.insert(
            window,
            WindowScope {
                generation,
                network,
                files: files.clone(),
                closed: false,
                _lease: lease,
            },
        );
        drop(state);
        self.auth.open_window(window, generation, browser);
        self.storage.window_changed(window, Some(generation));
        Ok(files)
    }

    fn reap(state: &mut State) {
        state
            .scopes
            .retain(|_, s| !s.closed || !s.network.is_quiescent() || !s.files.is_quiescent());
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
                let scope = state.scopes.get(&id).ok_or(ErrorCode::StaleHandle)?;
                if scope.closed || generation.is_some_and(|g| g != scope.generation) {
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
            let Some(scope) = state.scopes.get_mut(&window).filter(|s| !s.closed) else {
                return;
            };
            if let Some(generation) = generation {
                scope.generation = generation;
            } else {
                scope.closed = true;
            }
            (scope.network.clone(), scope.files.clone())
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
            state
                .scopes
                .values_mut()
                .map(|s| {
                    s.closed = true;
                    (s.network.clone(), s.files.clone())
                })
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
                .scopes
                .values()
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
