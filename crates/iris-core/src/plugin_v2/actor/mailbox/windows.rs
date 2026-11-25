use super::*;

fn same(a: Generation, b: Generation) -> bool {
    a.media == b.media && a.seek == b.seek
}
impl Mailbox {
    pub fn contexts(&self) -> (Generation, BTreeMap<Id, Generation>) {
        let state = self.lock();
        (
            state.generation,
            state
                .windows
                .iter()
                .map(|(id, scope)| (*id, scope.generation))
                .collect(),
        )
    }
    pub fn is_current(&self, window: Option<Id>, generation: Generation) -> bool {
        let state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return false;
        }
        match window {
            Some(id) => state
                .windows
                .get(&id)
                .is_some_and(|scope| scope.ready && scope.generation == generation),
            None => same(state.generation, generation),
        }
    }
    pub fn is_session_current(&self, generation: Generation) -> Option<Generation> {
        let state = self.lock();
        if self.cancel.load(Ordering::Acquire) || state.generation.media != generation.media {
            None
        } else {
            Some(state.generation)
        }
    }
    /// Window IDs come from the host's global monotonic allocator, never reused.
    pub fn open_window(&self, window: Id, generation: Generation) -> Result<(), ErrorCode> {
        if self.role != Role::Control || window.0 == 0 {
            return Err(ErrorCode::Denied);
        }
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
        )?;
        {
            let mut state = self.lock();
            if self.cancel.load(Ordering::Acquire) {
                return Err(ErrorCode::Cancelled);
            }
            if state.windows.contains_key(&window) {
                return Err(ErrorCode::BadState);
            }
            if state.windows.len() >= 4 || state.closed.len() >= 8 {
                return Err(ErrorCode::QuotaExceeded);
            }
            state.windows.insert(
                window,
                WindowContext {
                    generation,
                    cancel: HttpCancel::default(),
                    ready: false,
                    _lease: lease,
                },
            );
        }
        // Executor changes run outside the mailbox lock; cancellation can wake
        // a completing operation. Admission remains closed during this interval.
        self.executor.window_changed(window, Some(generation));
        let mut state = self.lock();
        let scope = state
            .windows
            .get_mut(&window)
            .ok_or(ErrorCode::StaleHandle)?;
        if scope.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        scope.ready = true;
        Ok(())
    }
    pub fn advance_window(&self, window: Id, generation: Generation) -> Result<(), ErrorCode> {
        let cancel = {
            let mut state = self.lock();
            if self.cancel.load(Ordering::Acquire) {
                return Err(ErrorCode::Cancelled);
            }
            let scope = state
                .windows
                .get_mut(&window)
                .ok_or(ErrorCode::StaleHandle)?;
            if !scope.ready {
                return Err(ErrorCode::Backpressure);
            }
            let old = scope.generation;
            // A new medium starts its own seek/context counters. Within one
            // medium both counters remain monotonic.
            if generation.media < old.media
                || generation == old
                || (generation.media == old.media
                    && (generation.seek < old.seek || generation.context < old.context))
            {
                return Err(ErrorCode::StaleHandle);
            }
            scope.generation = generation;
            scope.ready = false;
            let cancel = std::mem::take(&mut scope.cancel);
            state.events.retain(|m| m.window() != Some(window));
            state.completions.retain(|m| m.window() != Some(window));
            state.sweep = true;
            cancel
        };
        cancel.cancel();
        self.executor.window_changed(window, Some(generation));
        let mut state = self.lock();
        let scope = state
            .windows
            .get_mut(&window)
            .ok_or(ErrorCode::StaleHandle)?;
        if scope.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        scope.ready = true;
        self.changed.notify_one();
        Ok(())
    }
    pub fn close_window(&self, window: Id) -> Result<(), ErrorCode> {
        let cancel = {
            let mut state = self.lock();
            let scope = state
                .windows
                .remove(&window)
                .ok_or(ErrorCode::StaleHandle)?;
            state.events.retain(|m| m.window() != Some(window));
            state.completions.retain(|m| m.window() != Some(window));
            state.sweep = true;
            let generation = state.generation;
            // The window's existing reservation pays for its final notification.
            // New windows are backpressured while close callbacks accumulate.
            state.closed.push_back(Message::Closed {
                window,
                generation,
                _lease: scope._lease,
            });
            scope.cancel
        };
        cancel.cancel();
        self.executor.window_changed(window, None);
        self.changed.notify_one();
        (self.wake)();
        Ok(())
    }
    pub fn send_window(
        &self,
        window: Id,
        generation: Generation,
        bytes: Vec<u8>,
    ) -> Result<(), ErrorCode> {
        self.send_cancellable(window, generation, bytes, None)
    }
    pub fn send_cancellable(
        &self,
        window: Id,
        generation: Generation,
        bytes: Vec<u8>,
        request_cancel: Option<HttpCancel>,
    ) -> Result<(), ErrorCode> {
        if request_cancel
            .as_ref()
            .is_some_and(HttpCancel::is_cancelled)
        {
            return Err(ErrorCode::Cancelled);
        }
        let memory = self.ledger.reserve(
            &self.owner,
            &[(Resource::HostBytes, (MESSAGE_LIMIT * 32) as u64)],
        )?;
        let body: iris_api::v2::HostEvent<serde_json::Value> =
            iris_plugin_sdk::json::decode(&bytes, MESSAGE_LIMIT - 512)?;
        if matches!(
            body,
            iris_api::v2::HostEvent::Window { .. }
                | iris_api::v2::HostEvent::Init { .. }
                | iris_api::v2::HostEvent::Shutdown
                | iris_api::v2::HostEvent::WindowClosed { .. }
        ) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut state = self.lock();
        if self.cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        let scope = state.windows.get(&window).ok_or(ErrorCode::StaleHandle)?;
        if scope.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        if !scope.ready || state.events.len() >= 32 {
            return Err(ErrorCode::Backpressure);
        }
        state.events.push_back(Message::Event {
            bytes,
            initialization: false,
            request_cancel,
            generation,
            window: Some(window),
            read: None,
            attachment: None,
            _memory: memory,
        });
        self.changed.notify_one();
        Ok(())
    }
}
