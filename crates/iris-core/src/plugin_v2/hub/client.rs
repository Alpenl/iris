use super::*;
use crate::plugin_v2::requests::{BrowseTarget, RequestTarget};

pub struct Client {
    pub(super) hub: Hub,
    pub(super) inbox: Arc<Inbox>,
}
impl Client {
    pub fn window_id(&self) -> Id {
        self.inbox.id
    }
    pub fn generation(&self) -> Generation {
        self.inbox.generation()
    }
    pub fn update_snapshot(&self, snapshot: PlaybackSnapshot) -> Result<(), ErrorCode> {
        crate::plugin_v2::json::encode(&snapshot, iris_api::v2::MESSAGE_LIMIT - 1024)?;
        self.queue(Command::Snapshot {
            window: self.inbox.id,
            snapshot,
        })
    }
    pub fn catalog(&self) -> Arc<Catalog> {
        self.hub.catalog()
    }
    pub fn access(&self, plugin: &str) -> Result<super::super::runtime::WindowAccess, ErrorCode> {
        let binding = self.binding(plugin)?;
        let access = self
            .inbox
            .access
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&binding.installed.installation_id)
            .cloned()
            .ok_or(ErrorCode::Backpressure)?;
        if access.generation != self.generation() || self.inbox.closed.load(Ordering::Acquire) {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(access)
    }
    pub fn is_ready(&self, plugin: &str) -> bool {
        self.catalog()
            .plugins
            .iter()
            .find(|p| p.id == plugin && p.status == PluginStatus::Ready)
            .and_then(|p| p.binding.as_ref())
            .is_some_and(|b| {
                b.is_live()
                    && self
                        .inbox
                        .ready
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&b.installed.installation_id)
            })
    }
    pub(crate) fn ready_installations(&self) -> BTreeSet<String> {
        self.inbox
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn poll(&self) -> Vec<Notice> {
        self.inbox.wake_pending.store(false, Ordering::Release);
        let generation = self.generation();
        self.inbox
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter_map(|mut notice| {
                let keep = match &mut notice {
                    Notice::Commands(batch) => {
                        if !batch.binding.is_live() || batch.update.is_cancelled() {
                            false
                        } else if batch.update.window.is_none()
                            || batch.update.generation == generation
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
                            && self
                                .inbox
                                .requests
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
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
                keep.then_some(notice)
            })
            .collect()
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
        super::super::validation::identifier(plugin)?;
        crate::plugin_v2::json::encode(&event, iris_api::v2::MESSAGE_LIMIT - 1024)?;
        self.queue(Command::Send {
            window: self.inbox.id,
            plugin: plugin.into(),
            event,
            generation: self.generation(),
            request,
        })
    }
    pub fn advance(&self, generation: Generation) -> Result<(), ErrorCode> {
        let old = self.generation();
        if generation.media < old.media
            || (generation.media == old.media
                && (generation.seek < old.seek || generation.context < old.context))
        {
            return Err(ErrorCode::StaleHandle);
        }
        if generation == old {
            return Ok(());
        }
        let mut gate = self.hub.0.0.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.shutdown || self.inbox.closed.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        // One reserved, coalescing lifecycle command per admitted window. A
        // flooded ordinary queue must not stop native playback or seek cleanup.
        gate.commands
            .retain(|c| !matches!(c, Command::Advance { window, .. } if *window == self.inbox.id));
        gate.commands.push_back(Command::Advance {
            window: self.inbox.id,
            generation,
        });
        *self
            .inbox
            .generation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = generation;
        if generation.media != old.media {
            for access in self
                .inbox
                .access
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
            {
                access.cancel.cancel();
            }
        }
        gate.wake = true;
        self.hub.0.0.changed.notify_one();
        drop(gate);
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
        let mut requests = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for owner in owners {
            if generation.context != old.context {
                requests.cancel_owner(&owner);
            } else if generation.media != old.media {
                requests.cancel_resolves(&owner);
            }
        }
        Ok(())
    }
    fn binding(&self, plugin: &str) -> Result<Arc<Binding>, ErrorCode> {
        self.catalog()
            .plugins
            .iter()
            .find(|p| p.id == plugin && p.enabled)
            .and_then(|p| p.binding.clone())
            .filter(|b| b.is_live())
            .ok_or(ErrorCode::StaleHandle)
    }
    pub fn browse(&self, plugin: &str, target: BrowseTarget) -> Result<(Id, Id), ErrorCode> {
        let binding = self.binding(plugin)?;
        let (view, ticket) = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open_view(
                &binding.installed.installation_id,
                self.generation().context,
                target.clone(),
            )?;
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
            let _ = self
                .inbox
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .close_view(&binding.installed.installation_id, view);
            return Err(error);
        }
        Ok((view, id))
    }
    pub fn next_page(&self, plugin: &str, view: Id, cursor: &str) -> Result<Id, ErrorCode> {
        let binding = self.binding(plugin)?;
        let owner = &binding.installed.installation_id;
        let (ticket, target) = {
            let mut requests = self
                .inbox
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let target = requests.target(owner, view)?.clone();
            (requests.next_page(owner, view, cursor)?, target)
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
        self.finish_send(owner, id, result)?;
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
        let binding = self.binding(plugin)?;
        let owner = &binding.installed.installation_id;
        let ticket = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .issue(owner, self.generation().context, target)?;
        let id = ticket.id;
        let result = self.send_request(plugin, event(id), Some(ticket));
        self.finish_send(owner, id, result)?;
        Ok(id)
    }
    fn finish_send(
        &self,
        owner: &str,
        id: Id,
        result: Result<(), ErrorCode>,
    ) -> Result<(), ErrorCode> {
        if result.is_err() {
            let _ = self
                .inbox
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fail(owner, id, self.generation().context);
        }
        result
    }
    pub fn close_view(&self, plugin: &str, view: Id) -> Result<(), ErrorCode> {
        let binding = self.binding(plugin)?;
        let ids = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .close_view(&binding.installed.installation_id, view)?;
        self.cancel_notices(plugin, ids);
        Ok(())
    }
    pub fn cancel_request(&self, plugin: &str, request: Id) -> Result<(), ErrorCode> {
        let binding = self.binding(plugin)?;
        self.inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_request(&binding.installed.installation_id, request)?;
        self.cancel_notices(plugin, vec![request]);
        Ok(())
    }
    pub fn cancel_plugin_requests(&self, plugin: &str) -> Result<(), ErrorCode> {
        let binding = self.binding(plugin)?;
        let ids = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_owner(&binding.installed.installation_id);
        self.cancel_notices(plugin, ids);
        Ok(())
    }
    fn cancel_notices(&self, plugin: &str, ids: Vec<Id>) {
        let mut cancelled = self
            .inbox
            .cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = cancelled.entry(plugin.into()).or_default();
        // A plugin is stopped if cancellations outpace its callback queue.
        // Id(0), which is never issued as a request, is the overflow marker.
        if entry.len() + ids.len() > 128 {
            entry.clear();
            entry.insert(Id(0));
        } else if !entry.contains(&Id(0)) {
            entry.extend(ids);
        }
        drop(cancelled);
        self.hub.0.0.signal();
    }
    pub fn set_enabled(&self, plugin: &str, enabled: bool) -> Result<(), ErrorCode> {
        super::super::validation::identifier(plugin)?;
        self.queue(Command::Enabled {
            plugin: plugin.into(),
            enabled,
        })
    }
    pub fn uninstall(&self, plugin: &str, keep_data: bool) -> Result<(), ErrorCode> {
        super::super::validation::identifier(plugin)?;
        self.queue(Command::Uninstall {
            window: self.inbox.id,
            plugin: plugin.into(),
            keep_data,
        })
    }
    pub fn reload(&self) -> Result<(), ErrorCode> {
        self.queue(Command::Reload)
    }
    pub fn review(&self, path: PathBuf) -> Result<Id, ErrorCode> {
        if path.as_os_str().len() > 32_768 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let id = Id(super::super::instance::next_id()?);
        self.queue(Command::Review {
            window: self.inbox.id,
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
        self.queue(Command::Approve {
            window: self.inbox.id,
            id,
            grants,
            keep_identity,
        })
    }
    pub fn dismiss_review(&self, id: Id) -> Result<(), ErrorCode> {
        self.queue(Command::DismissReview {
            window: self.inbox.id,
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
        self.queue(Command::ConfirmOrigin {
            window: self.inbox.id,
            id,
            allow,
            insecure_auth,
        })
    }
    fn queue(&self, command: Command) -> Result<(), ErrorCode> {
        if self.inbox.closed.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        self.hub.0.0.enqueue(command)
    }
    pub fn shutdown(&self) {
        if !self.inbox.closed.swap(true, Ordering::AcqRel) {
            // Lifecycle cancellation has its own slot; a full ordinary queue
            // cannot prevent closing a window or release admission prematurely.
            self.inbox
                .notices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.inbox
                .access
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.hub.0.0.signal();
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}
