use super::{Binding, Catalog, Command, Hub, Inbox, Notice, PluginStatus};
use crate::json_error;
use crate::requests::{BrowseTarget, RequestTarget, RequestTicket};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use iris_api::v2::{Capability, ErrorCode, Generation, HostEvent, Id, source::PlaybackSnapshot};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(super) enum ClientState {
    Connected {
        hub: Hub,
        inbox: Arc<Inbox>,
    },
    Unavailable {
        error: Mutex<Option<ErrorCode>>,
        catalog: Arc<Catalog>,
    },
}

pub struct Client {
    pub(super) state: ClientState,
}

impl Client {
    pub(super) fn connected(hub: Hub, inbox: Arc<Inbox>) -> Self {
        Self {
            state: ClientState::Connected { hub, inbox },
        }
    }

    /// A lightweight error-only client with no workers, ledger, condvar, or disk access.
    pub(crate) fn unavailable(code: ErrorCode) -> (Self, UnboundedReceiver<()>) {
        let (wake, rx) = unbounded();
        let _ = wake.unbounded_send(());
        let catalog = Arc::new(Catalog {
            revision: 0,
            plugins: Vec::new(),
        });
        (
            Self {
                state: ClientState::Unavailable {
                    error: Mutex::new(Some(code)),
                    catalog,
                },
            },
            rx,
        )
    }

    fn live(&self) -> Result<(&Hub, &Arc<Inbox>), ErrorCode> {
        match &self.state {
            ClientState::Connected { hub, inbox } => Ok((hub, inbox)),
            ClientState::Unavailable { .. } => Err(ErrorCode::Cancelled),
        }
    }

    pub fn window_id(&self) -> Id {
        match &self.state {
            ClientState::Connected { inbox, .. } => inbox.id,
            ClientState::Unavailable { .. } => Id(0),
        }
    }

    pub fn generation(&self) -> Generation {
        match &self.state {
            ClientState::Connected { inbox, .. } => inbox.generation(),
            ClientState::Unavailable { .. } => Generation::default(),
        }
    }

    pub fn update_snapshot(&self, snapshot: PlaybackSnapshot) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        iris_core::bounded_json::encode(&snapshot, iris_api::v2::MESSAGE_LIMIT - 1024)
            .map_err(json_error)?;
        self.queue(Command::Snapshot {
            window: inbox.id,
            snapshot,
        })
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        match &self.state {
            ClientState::Connected { hub, .. } => hub.catalog(),
            ClientState::Unavailable { catalog, .. } => catalog.clone(),
        }
    }

    pub fn access(&self, plugin: &str) -> Result<super::super::runtime::WindowAccess, ErrorCode> {
        let (_, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let snap = inbox.snapshot();
        if snap.closed {
            return Err(ErrorCode::StaleHandle);
        }
        let access = snap
            .access
            .get(&binding.installed.installation_id)
            .cloned()
            .ok_or(ErrorCode::Backpressure)?;
        if access.generation != snap.generation {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(access)
    }

    pub fn is_ready(&self, plugin: &str) -> bool {
        let ClientState::Connected { inbox, .. } = &self.state else {
            return false;
        };
        self.catalog()
            .plugins
            .iter()
            .find(|p| p.id == plugin && p.status == PluginStatus::Ready)
            .and_then(|p| p.binding.as_ref())
            .is_some_and(|b| {
                if !b.is_live() {
                    return false;
                }
                let state = inbox.lock();
                !state.closed && state.ready.contains(&b.installed.installation_id)
            })
    }

    pub(crate) fn ready_installations(&self) -> BTreeSet<String> {
        match &self.state {
            ClientState::Connected { inbox, .. } => inbox.lock().ready.clone(),
            ClientState::Unavailable { .. } => BTreeSet::new(),
        }
    }

    pub(crate) fn notify_wake(&self) {
        if let ClientState::Connected { inbox, .. } = &self.state {
            inbox.notify();
        }
    }

    pub(crate) fn waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        match &self.state {
            ClientState::Connected { inbox, .. } => {
                let weak = Arc::downgrade(inbox);
                Some(Arc::new(move || {
                    if let Some(inbox) = weak.upgrade() {
                        inbox.notify();
                    }
                }))
            }
            ClientState::Unavailable { .. } => None,
        }
    }

    pub fn poll(&self) -> Vec<Notice> {
        self.poll_bounded(usize::MAX, usize::MAX)
    }

    /// Drains up to `max_notices` live notices and `max_commands` live guest commands,
    /// discarding stale-generation entries immediately while leaving unread notices in
    /// [`Inbox`] so backpressure (`notices.len() >= 32`) remains enforced.
    pub(crate) fn poll_bounded(&self, max_notices: usize, max_commands: usize) -> Vec<Notice> {
        let inbox = match &self.state {
            ClientState::Connected { inbox, .. } => inbox,
            ClientState::Unavailable { error, .. } => {
                if max_notices == 0 {
                    return Vec::new();
                }
                return error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .map(|code| Notice::Error {
                        plugin: None,
                        request: None,
                        code,
                        message: None,
                    })
                    .into_iter()
                    .collect();
            }
        };
        let mut state = inbox.lock();
        state.wake_pending = false;
        let generation = state.generation;
        let mut out = Vec::new();
        let mut command_count = 0usize;
        while out.len() < max_notices
            && let Some(mut notice) = state.notices.pop_front()
        {
            let keep = match &mut notice {
                Notice::Commands(batch) => {
                    if !batch.binding.is_live() || batch.update.is_cancelled() {
                        false
                    } else if batch.update.window.is_none() || batch.update.generation == generation
                    {
                        true
                    } else if batch.update.generation.context == generation.context {
                        batch.indices.retain(|&index| {
                            super::dispatch::is_slot_command(&batch.update.commands[index])
                        });
                        !batch.indices.is_empty()
                    } else {
                        false
                    }
                }
                Notice::Page {
                    binding,
                    generation: g,
                    page,
                    ..
                } => {
                    binding.is_live()
                        && g.context == generation.context
                        && state
                            .requests
                            .target(&binding.installed.installation_id, page.view)
                            .is_ok()
                }
                Notice::Playback {
                    binding,
                    generation: g,
                    ticket,
                    ..
                } => binding.is_live() && *g == generation && !ticket.cancel.is_cancelled(),
                Notice::Browser {
                    binding,
                    generation: g,
                    ..
                } => binding.is_live() && *g == generation,
                Notice::Origin(review) => {
                    review.binding.is_live() && review.generation == generation
                }
                _ => true,
            };
            if !keep {
                continue;
            }
            if let Notice::Commands(batch) = &mut notice {
                let remaining = max_commands.saturating_sub(command_count);
                if remaining == 0 {
                    state.notices.push_front(notice);
                    break;
                }
                if batch.indices.len() > remaining {
                    let rest = batch.indices.split_off(remaining);
                    let head = Notice::Commands(super::Commands {
                        binding: batch.binding.clone(),
                        update: batch.update.clone(),
                        indices: std::mem::take(&mut batch.indices),
                    });
                    batch.indices = rest;
                    state.notices.push_front(notice);
                    out.push(head);
                    break;
                }
                command_count += batch.indices.len();
            }
            out.push(notice);
        }
        let rearm = !state.notices.is_empty() && !state.closed;
        if rearm {
            state.wake_pending = true;
        }
        drop(state);
        if rearm {
            let _ = inbox.wake.unbounded_send(());
        }
        out
    }

    pub fn send(&self, plugin: &str, event: HostEvent<serde_json::Value>) -> Result<(), ErrorCode> {
        if matches!(
            event,
            HostEvent::SourceBrowse { .. }
                | HostEvent::SourceResolve { .. }
                | HostEvent::ResolveUrl { .. }
                | HostEvent::RequestCancelled { .. }
        ) {
            return Err(ErrorCode::Denied);
        }
        self.send_request(plugin, event, None)
    }

    fn send_request(
        &self,
        plugin: &str,
        event: HostEvent<serde_json::Value>,
        request: Option<Arc<RequestTicket>>,
    ) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        super::super::validation::identifier(plugin)?;
        iris_core::bounded_json::encode(&event, iris_api::v2::MESSAGE_LIMIT - 1024)
            .map_err(json_error)?;
        let generation = {
            let state = inbox.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            state.generation
        };
        self.queue(Command::Send {
            window: inbox.id,
            plugin: plugin.into(),
            event,
            generation,
            request,
        })
    }

    pub fn advance(&self, generation: Generation) -> Result<(), ErrorCode> {
        let (hub, inbox) = self.live()?;
        let owners: Vec<_> = self
            .catalog()
            .plugins
            .iter()
            .filter_map(|p| {
                p.binding
                    .as_ref()
                    .map(|b| b.installed.installation_id.clone())
            })
            .collect();
        let mut gate = hub.0.0.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.shutdown {
            return Err(ErrorCode::Cancelled);
        }
        let accesses_to_cancel = {
            let mut state = inbox.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            let old = state.generation;
            if generation.media < old.media
                || (generation.media == old.media
                    && (generation.seek < old.seek || generation.context < old.context))
            {
                return Err(ErrorCode::StaleHandle);
            }
            if generation == old {
                return Ok(());
            }
            // One reserved, coalescing lifecycle command per admitted window. A
            // flooded ordinary queue must not stop native playback or seek cleanup.
            gate.commands
                .retain(|c| !matches!(c, Command::Advance { window, .. } if *window == inbox.id));
            gate.commands.push_back(Command::Advance {
                window: inbox.id,
                generation,
            });
            state.generation = generation;
            for owner in &owners {
                if generation.context != old.context {
                    state.requests.cancel_owner(owner);
                } else if generation.media != old.media {
                    state.requests.cancel_resolves(owner);
                }
            }
            if generation.media != old.media {
                state
                    .access
                    .values()
                    .map(|access| access.cancel.clone())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        gate.wake = true;
        hub.0.0.changed.notify_one();
        drop(gate);
        for cancel in accesses_to_cancel {
            cancel.cancel();
        }
        Ok(())
    }

    fn binding(&self, plugin: &str) -> Result<Arc<Binding>, ErrorCode> {
        self.catalog()
            .plugins
            .iter()
            .find(|p| {
                p.id == plugin
                    && p.enabled
                    && !matches!(
                        p.status,
                        PluginStatus::Failed(_) | PluginStatus::Stopping | PluginStatus::Disabled
                    )
            })
            .and_then(|p| p.binding.clone())
            .filter(|b| b.is_live())
            .ok_or(ErrorCode::StaleHandle)
    }

    pub fn browse(&self, plugin: &str, target: BrowseTarget) -> Result<(Id, Id), ErrorCode> {
        let (_, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let (view, ticket) = {
            let mut state = inbox.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            let context_gen = state.generation.context;
            state.requests.open_view(
                &binding.installed.installation_id,
                context_gen,
                target.clone(),
            )?
        };
        let event = HostEvent::SourceBrowse {
            request_id: ticket.id,
            source_id: target.source,
            folder_id: target.folder,
            page: None,
            query: target.query,
            filters: target.filters,
        };
        let id = ticket.id;
        if let Err(error) = self.send_request(plugin, event, Some(ticket)) {
            let _ = inbox
                .lock()
                .requests
                .close_view(&binding.installed.installation_id, view);
            return Err(error);
        }
        Ok((view, id))
    }

    pub fn next_page(&self, plugin: &str, view: Id, cursor: &str) -> Result<Id, ErrorCode> {
        let (_, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let owner = &binding.installed.installation_id;
        let (ticket, target) = {
            let mut state = inbox.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            let target = state.requests.target(owner, view)?.clone();
            (state.requests.next_page(owner, view, cursor)?, target)
        };
        let id = ticket.id;
        let result = self.send_request(
            plugin,
            HostEvent::SourceBrowse {
                request_id: id,
                source_id: target.source,
                folder_id: target.folder,
                page: Some(cursor.into()),
                query: target.query,
                filters: target.filters,
            },
            Some(ticket),
        );
        self.finish_send(inbox, owner, id, result)?;
        Ok(id)
    }

    pub fn resolve(
        &self,
        plugin: &str,
        source: &str,
        folder: Option<String>,
        entry: &str,
        variant: Option<String>,
    ) -> Result<Id, ErrorCode> {
        self.issue(
            plugin,
            RequestTarget::Resolve {
                source: source.into(),
                entry: entry.into(),
                variant: variant.clone(),
            },
            |id| HostEvent::SourceResolve {
                request_id: id,
                source_id: source.into(),
                folder_id: folder,
                entry_id: entry.into(),
                variant,
            },
        )
    }

    pub fn resolve_url(&self, plugin: &str, url: &str) -> Result<Id, ErrorCode> {
        self.issue(plugin, RequestTarget::Url { url: url.into() }, |id| {
            HostEvent::ResolveUrl {
                request_id: id,
                url: url.into(),
            }
        })
    }

    fn issue(
        &self,
        plugin: &str,
        target: RequestTarget,
        event: impl FnOnce(Id) -> HostEvent<serde_json::Value>,
    ) -> Result<Id, ErrorCode> {
        let (_, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let owner = &binding.installed.installation_id;
        let ticket = {
            let mut state = inbox.lock();
            if state.closed {
                return Err(ErrorCode::Cancelled);
            }
            let context_gen = state.generation.context;
            state.requests.issue(owner, context_gen, target)?
        };
        let id = ticket.id;
        let result = self.send_request(plugin, event(id), Some(ticket));
        self.finish_send(inbox, owner, id, result)?;
        Ok(id)
    }

    fn finish_send(
        &self,
        inbox: &Arc<Inbox>,
        owner: &str,
        id: Id,
        result: Result<(), ErrorCode>,
    ) -> Result<(), ErrorCode> {
        if result.is_err() {
            let mut state = inbox.lock();
            let context_gen = state.generation.context;
            let _ = state.requests.fail(owner, id, context_gen);
        }
        result
    }

    pub fn close_view(&self, plugin: &str, view: Id) -> Result<(), ErrorCode> {
        let (hub, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let ids = inbox
            .lock()
            .requests
            .close_view(&binding.installed.installation_id, view)?;
        Self::cancel_notices(hub, inbox, plugin, ids);
        Ok(())
    }

    pub fn cancel_request(&self, plugin: &str, request: Id) -> Result<(), ErrorCode> {
        let (hub, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        inbox
            .lock()
            .requests
            .cancel_request(&binding.installed.installation_id, request)?;
        Self::cancel_notices(hub, inbox, plugin, vec![request]);
        Ok(())
    }

    pub fn cancel_plugin_requests(&self, plugin: &str) -> Result<(), ErrorCode> {
        let (hub, inbox) = match &self.state {
            ClientState::Connected { hub, inbox } => (hub, inbox),
            ClientState::Unavailable { .. } => return Err(ErrorCode::StaleHandle),
        };
        let binding = self.binding(plugin)?;
        let ids = inbox
            .lock()
            .requests
            .cancel_owner(&binding.installed.installation_id);
        Self::cancel_notices(hub, inbox, plugin, ids);
        Ok(())
    }

    fn cancel_notices(hub: &Hub, inbox: &Arc<Inbox>, plugin: &str, ids: Vec<Id>) {
        {
            let mut state = inbox.lock();
            let entry = state.cancelled.entry(plugin.into()).or_default();
            // A plugin is stopped if cancellations outpace its callback queue.
            // Id(0), which is never issued as a request, is the overflow marker.
            if entry.len() + ids.len() > 128 {
                entry.clear();
                entry.insert(Id(0));
            } else if !entry.contains(&Id(0)) {
                entry.extend(ids);
            }
        }
        hub.0.0.signal();
    }

    pub fn set_enabled(&self, plugin: &str, enabled: bool) -> Result<(), ErrorCode> {
        super::super::validation::identifier(plugin)?;
        self.queue(Command::Enabled {
            plugin: plugin.into(),
            enabled,
        })
    }

    pub fn uninstall(&self, plugin: &str, keep_data: bool) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        super::super::validation::identifier(plugin)?;
        self.queue(Command::Uninstall {
            window: inbox.id,
            plugin: plugin.into(),
            keep_data,
        })
    }

    pub fn reload(&self) -> Result<(), ErrorCode> {
        self.queue(Command::Reload { disabled: None })
    }

    pub fn reload_with_disabled(&self, disabled: BTreeSet<String>) -> Result<(), ErrorCode> {
        self.queue(Command::Reload {
            disabled: Some(disabled),
        })
    }

    pub fn review(&self, path: PathBuf) -> Result<Id, ErrorCode> {
        let (_, inbox) = self.live()?;
        if path.as_os_str().len() > 32_768 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let id = Id(super::super::instance::next_id()?);
        self.queue(Command::Review {
            window: inbox.id,
            id,
            path,
        })?;
        Ok(id)
    }

    pub fn approve(
        &self,
        id: Id,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    ) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        self.queue(Command::Approve {
            window: inbox.id,
            id,
            grants,
            keep_identity,
        })
    }

    pub fn dismiss_review(&self, id: Id) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        self.queue(Command::DismissReview {
            window: inbox.id,
            id,
        })
    }

    /// Called by the host consent sheet, never by a plugin-defined widget.
    pub fn confirm_origin(
        &self,
        id: Id,
        allow: bool,
        insecure_auth: bool,
    ) -> Result<(), ErrorCode> {
        let (_, inbox) = self.live()?;
        self.queue(Command::ConfirmOrigin {
            window: inbox.id,
            id,
            allow,
            insecure_auth,
        })
    }

    fn queue(&self, command: Command) -> Result<(), ErrorCode> {
        let (hub, inbox) = self.live()?;
        if inbox.is_closed() {
            return Err(ErrorCode::Cancelled);
        }
        hub.0.0.enqueue(command)
    }

    pub fn shutdown(&self) {
        match &self.state {
            ClientState::Connected { hub, inbox } => {
                let first_close = {
                    let mut state = inbox.lock();
                    if state.closed {
                        false
                    } else {
                        state.closed = true;
                        state.notices.clear();
                        state.access.clear();
                        true
                    }
                };
                if first_close {
                    // Lifecycle cancellation has its own slot; a full ordinary queue
                    // cannot prevent closing a window or release admission prematurely.
                    hub.0.0.signal();
                }
            }
            ClientState::Unavailable { error, .. } => {
                error.lock().unwrap_or_else(|e| e.into_inner()).take();
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}
