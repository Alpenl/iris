//! Multiplex one installation's ControlStore without mixing window resources.
use super::*;
use crate::plugin_v2::ledger::{Ledger, Reservation, Resource};
use iris_api::v2::{Id, Role};
use std::sync::Mutex;

struct Scope {
    executor: Arc<dyn Executor>,
    generation: Generation,
    closed: bool,
    _lease: Reservation,
}
struct State {
    stopped: bool,
    scopes: BTreeMap<Id, Scope>,
}
pub struct WindowServices {
    owner: String,
    ledger: Ledger,
    global: Arc<dyn Executor>,
    state: Mutex<State>,
}
impl WindowServices {
    pub fn new(owner: String, ledger: Ledger, global: Arc<dyn Executor>) -> Self {
        Self {
            owner,
            ledger,
            global,
            state: Mutex::new(State {
                stopped: false,
                scopes: BTreeMap::new(),
            }),
        }
    }
    /// Prepare the concrete services off the UI thread, then register them
    /// before Actor::open_window. IDs are host-issued and never reused.
    pub fn register(
        &self,
        window: Id,
        generation: Generation,
        executor: Arc<dyn Executor>,
    ) -> Result<(), ErrorCode> {
        if window.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
        )?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::reap(&mut state);
        if state.stopped {
            return Err(ErrorCode::Cancelled);
        }
        if state.scopes.contains_key(&window) {
            return Err(ErrorCode::BadState);
        }
        // Retired executors continue to occupy capacity until all I/O exits.
        if state.scopes.len() >= 8 || state.scopes.values().filter(|s| !s.closed).count() >= 4 {
            return Err(ErrorCode::Backpressure);
        }
        state.scopes.insert(
            window,
            Scope {
                executor,
                generation,
                closed: false,
                _lease: lease,
            },
        );
        Ok(())
    }
    fn reap(state: &mut State) {
        state
            .scopes
            .retain(|_, s| !s.closed || !s.executor.is_quiescent());
    }
    fn executor(
        &self,
        window: Option<Id>,
        generation: Option<Generation>,
    ) -> Result<Arc<dyn Executor>, ErrorCode> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return Err(ErrorCode::Cancelled);
        }
        match window {
            None => Ok(self.global.clone()),
            Some(id) => {
                let scope = state.scopes.get(&id).ok_or(ErrorCode::StaleHandle)?;
                if scope.closed || generation.is_some_and(|g| g != scope.generation) {
                    return Err(ErrorCode::StaleHandle);
                }
                Ok(scope.executor.clone())
            }
        }
    }
}
impl Executor for WindowServices {
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if context.owner != self.owner || context.role != Role::Control {
            return Err(ErrorCode::Denied);
        }
        self.executor(context.window, Some(context.generation))?
            .prepare(task, blobs, context)
    }
    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        self.close_in(None, handle)
    }
    fn close_in(&self, window: Option<Id>, handle: u64) -> Result<(), ErrorCode> {
        self.executor(window, None)?.close(handle)
    }
    fn window_changed(&self, window: Id, generation: Option<Generation>) {
        let executor = {
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
            scope.executor.clone()
        };
        // Never run a cancellation callback while holding the router lock.
        if let Some(generation) = generation {
            executor.generation_changed(generation);
        } else {
            executor.stop();
        }
    }
    fn generation_changed(&self, generation: Generation) {
        self.global.generation_changed(generation);
    }
    fn stop(&self) {
        let executors: Vec<_> = {
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
                    s.executor.clone()
                })
                .collect()
        };
        self.global.stop();
        for executor in executors {
            executor.stop();
        }
    }
    fn is_quiescent(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::reap(&mut state);
        self.global.is_quiescent() && state.scopes.values().all(|s| s.executor.is_quiescent())
    }
}
impl Drop for WindowServices {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
