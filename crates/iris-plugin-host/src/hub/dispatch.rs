use super::{
    Binding, Command, Commands, Notice, PluginStatus,
    jobs::{Work, guarded},
    worker::{Window, Worker},
};
use crate::actor::Update;
use crate::ledger::Resource;
use crate::requests::RequestTicket;
use iris_api::v2::{Capability, ErrorCode, Generation, HostCommand, HostEvent, Id, Role};
use std::{collections::BTreeSet, sync::Arc};

impl Worker {
    pub fn command(&mut self, command: Command) {
        if self.shutdown {
            return;
        }
        match command {
            Command::Snapshot { window, snapshot } => {
                if let Some(window) = self.active_window_mut(window) {
                    window.snapshot = Some(snapshot);
                }
            }
            Command::Open { inbox, snapshot } => {
                if self.window.is_some() {
                    self.close_active_window();
                }
                let (closed, generation) = {
                    let state = inbox.lock();
                    (state.closed, state.generation)
                };
                if closed {
                    return;
                }
                self.window = Some(Window {
                    inbox,
                    snapshot,
                    generation,
                });
                self.dirty = true;
            }
            Command::Advance { window, generation } => {
                let Some(state) = self.active_window_mut(window) else {
                    return;
                };
                let old_generation = state.generation;
                state.generation = generation;
                let inbox = state.inbox.clone();
                let mut failed = Vec::new();
                let mut refreshed_access = Vec::new();
                for (id, plugin) in &mut self.plugins {
                    if plugin.attached == Some(window)
                        && let Some(control) = &mut plugin.control
                    {
                        if plugin.failure.is_some() {
                            if generation.media != old_generation.media {
                                let _ = control.close_window(window);
                                plugin.attached = None;
                            }
                            continue;
                        }
                        match control
                            .advance_window(window, generation)
                            .and_then(|_| control.window_access(window))
                        {
                            Ok(access) => refreshed_access.push(access),
                            Err(error) => failed.push((id.clone(), error)),
                        }
                    }
                }
                if !refreshed_access.is_empty() {
                    let mut inbox_state = inbox.lock();
                    for access in refreshed_access {
                        inbox_state.access.insert(access.owner.clone(), access);
                    }
                }
                for (id, error) in failed {
                    self.fail_plugin(&id, error);
                }
                if let Some(state) = self.active_window(window) {
                    state.inbox.notify();
                }
            }
            Command::Send {
                window,
                plugin,
                event,
                generation,
                request,
            } => {
                let request_id = request.as_ref().map(|r| r.id);
                let result = self.send(window, &plugin, event, generation, request.as_ref());
                if let Err(error) = result {
                    let was_cancelled = request.as_ref().is_some_and(|r| r.cancel.is_cancelled());
                    if let Some(request) = request {
                        request.cancel.cancel();
                        if let Some(window) = self.active_window(window) {
                            let _ = window.inbox.lock().requests.fail(
                                &request.owner,
                                request.id,
                                request.generation,
                            );
                        }
                    }
                    // Stale/cancelled non-request events (e.g. a Tick or StateChanged
                    // racing with Advance) are expected and should not toast errors.
                    if !(matches!(error, ErrorCode::StaleHandle | ErrorCode::Cancelled)
                        && (request_id.is_none() || was_cancelled))
                    {
                        self.error(window, Some(plugin), request_id, error, None);
                    }
                }
            }
            Command::Enabled { plugin, enabled } => {
                if let Some(state) = self.plugins.get_mut(&plugin) {
                    state.enabled = enabled;
                    state.restart = enabled;
                    if enabled {
                        state.failure = None;
                    }
                    if !enabled {
                        state.stop();
                        self.config.disabled.insert(plugin);
                    } else {
                        self.config.disabled.remove(&plugin);
                    }
                    self.dirty = true;
                }
            }
            Command::Uninstall {
                window,
                plugin,
                keep_data,
            } => {
                if self.config.installer.is_bundled(&plugin) {
                    self.error(window, Some(plugin), None, ErrorCode::Denied, None);
                    return;
                }
                if self.uninstalling.contains(&plugin) {
                    return;
                }
                if let Some(state) = self.plugins.get_mut(&plugin) {
                    state.stop();
                    state.enabled = false;
                    state.restart = false;
                    state.status = PluginStatus::Stopping;
                    if let (Ok(record), Some(win)) = (&state.record, &self.window) {
                        win.inbox
                            .lock()
                            .requests
                            .cancel_owner(&record.installation_id);
                    }
                    self.dirty = true;
                }
                self.config.disabled.remove(&plugin);
                let installer = self.config.installer.clone();
                let setup = self.config.runtime.clone();
                let target = plugin.clone();
                match self.submit(false, "uninstall", move || {
                    let result = guarded(|| {
                        let removed = installer.uninstall(&target, keep_data)?;
                        if !keep_data && let Some(installed) = removed {
                            super::jobs::remove_installation_data(
                                &setup,
                                &installed.installation_id,
                            );
                        }
                        Ok(())
                    });
                    Work::Uninstalled {
                        window,
                        plugin: target,
                        result,
                    }
                }) {
                    Ok(()) => {
                        self.uninstalling.insert(plugin);
                    }
                    Err(error) => {
                        self.error(window, Some(plugin), None, error, None);
                    }
                }
            }
            Command::Reload { disabled } => {
                if let Some(disabled) = disabled {
                    self.config.disabled = disabled;
                }
                for (id, plugin) in &mut self.plugins {
                    let enabled = !self.config.disabled.contains(id);
                    plugin.enabled = enabled;
                    plugin.stop();
                    plugin.restart = enabled;
                    if !enabled {
                        plugin.failure = None;
                    }
                }
                self.rescan = true;
                self.dirty = true;
            }
            Command::SyncDisabled { disabled } => {
                self.config.disabled = disabled;
                for (id, plugin) in &mut self.plugins {
                    let enabled = !self.config.disabled.contains(id);
                    if plugin.enabled != enabled {
                        plugin.enabled = enabled;
                        plugin.restart = enabled;
                        if enabled {
                            plugin.failure = None;
                        } else {
                            plugin.stop();
                        }
                        self.dirty = true;
                    }
                }
            }
            Command::Review { window, id, path } => {
                if let Err(error) = self.review(window, id, path) {
                    self.error(window, None, Some(id), error, None);
                }
            }
            Command::Approve {
                window,
                id,
                grants,
                keep_identity,
            } => {
                let result = self.approve(window, id, grants, keep_identity);
                if let Err(error) = result {
                    self.error(window, None, Some(id), error, None);
                }
            }
            Command::DismissReview { window, id } => {
                if self.reviews.get(&id).is_some_and(|r| r.window == window)
                    && let Some(review) = self.reviews.remove(&id)
                {
                    if review.review.is_some() {
                        self.review_slots.remove(&id);
                    }
                    review.cancel.cancel();
                }
            }
            Command::ConfirmOrigin {
                window,
                id,
                allow,
                insecure_auth,
            } => {
                if let Err(error) = self.confirm_origin(window, id, allow, insecure_auth) {
                    self.error(window, None, Some(id), error, None);
                }
            }
        }
    }

    fn send(
        &mut self,
        window: Id,
        plugin: &str,
        event: HostEvent<serde_json::Value>,
        generation: Generation,
        request: Option<&Arc<RequestTicket>>,
    ) -> Result<(), ErrorCode> {
        let state = self
            .active_window_mut(window)
            .ok_or(ErrorCode::StaleHandle)?;
        let (closed, inbox_gen) = {
            let s = state.inbox.lock();
            (s.closed, s.generation)
        };
        if closed || generation != state.generation || generation != inbox_gen {
            return Err(ErrorCode::StaleHandle);
        }
        if let HostEvent::MediaOpened { snapshot, .. } | HostEvent::StateChanged { snapshot, .. } =
            &event
        {
            state.snapshot = Some(snapshot.clone());
        }
        let plugin = self.plugins.get(plugin).ok_or(ErrorCode::StaleHandle)?;
        if let Some(error) = plugin.failure {
            return Err(error);
        }
        if !plugin.enabled || plugin.attached != Some(window) {
            return Err(ErrorCode::Backpressure);
        }
        let control = plugin.control.as_ref().ok_or(ErrorCode::Backpressure)?;
        if let Some(request) = request {
            if request.owner != control.installed().installation_id || request.cancel.is_cancelled()
            {
                return Err(ErrorCode::StaleHandle);
            }
            control.send_cancellable(window, event, request.cancel.clone())
        } else {
            control.send(window, event)
        }
    }

    fn approve(
        &mut self,
        window: Id,
        id: Id,
        grants: BTreeSet<Capability>,
        keep: bool,
    ) -> Result<(), ErrorCode> {
        let pending = self.reviews.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if pending.window != window {
            return Err(ErrorCode::Denied);
        }
        if pending.expires.is_expired() || self.active_window(window).is_none() {
            return Err(ErrorCode::StaleHandle);
        }
        if pending.review.is_none() {
            return Err(ErrorCode::Backpressure);
        }
        let lease = self.reserve_job_lease()?;
        let review = self.reviews.remove(&id).unwrap().review.unwrap();
        let is_bundled = review.is_bundled();
        let bootstrap = self.config.bootstrap.clone();
        let mut installer = self.config.installer.clone();
        let setup = self.config.runtime.clone();
        let result = self.submit_reserved(false, "install", lease, move || Work::Installed {
            window,
            id,
            result: guarded(|| {
                let (installed, superseded) =
                    review.approve_with_cleanup(&mut installer, grants, keep)?;
                for old_id in superseded {
                    super::jobs::remove_installation_data(&setup, &old_id);
                }
                if is_bundled && let Some(bootstrap) = bootstrap {
                    bootstrap.migrate_installed(&installed, &setup)?;
                }
                Ok(installed)
            }),
        });
        if result.is_err() {
            self.review_slots.remove(&id);
        }
        result
    }

    pub fn cancellations(&mut self) {
        let mut failed = Vec::new();
        if let Some(window) = &self.window {
            let window_id = window.inbox.id;
            let (faults, all) = {
                let mut state = window.inbox.lock();
                (
                    std::mem::take(&mut state.faults),
                    std::mem::take(&mut state.cancelled),
                )
            };
            failed.extend(faults);
            for (plugin_id, ids) in all {
                let Some(plugin) = self.plugins.get(&plugin_id) else {
                    continue;
                };
                let Some(control) = &plugin.control else {
                    continue;
                };
                if plugin.attached != Some(window_id) {
                    continue;
                }
                for id in ids {
                    let result = if id == Id(0) {
                        Err(ErrorCode::Backpressure)
                    } else {
                        control.send(window_id, HostEvent::RequestCancelled { request_id: id })
                    };
                    if let Err(error) = result {
                        failed.push((plugin_id.clone(), error));
                        break;
                    }
                }
            }
        }
        for (id, error) in failed {
            self.fail_plugin(&id, error);
        }
    }

    pub fn deliver(&mut self, plugin: &str, update: Update) {
        if update.is_cancelled() {
            return;
        }
        let Some(binding) = self.plugins[plugin].binding.clone().filter(|b| b.is_live()) else {
            return;
        };
        let Some((window, current_gen)) = self.window.as_ref().map(|w| (w.inbox.id, w.generation))
        else {
            return;
        };
        if update.window.is_some_and(|target| target != window) {
            return;
        }
        if update.window.is_some() && update.generation.context != current_gen.context {
            return;
        }
        let update = Arc::new(update);
        let same_media = update.window.is_none() || update.generation == current_gen;
        let mut indices = Vec::new();
        for (index, command) in update.commands.iter().enumerate() {
            let result = match command {
                HostCommand::SetSources { .. } | HostCommand::Log { .. } => Ok(()), // shared catalog / host logging owns these commands
                HostCommand::RequestOrigin { request_id, origin } => {
                    if !same_media {
                        Err(ErrorCode::StaleHandle)
                    } else {
                        self.request_origin(
                            window,
                            binding.clone(),
                            update.generation,
                            *request_id,
                            origin,
                        )
                    }
                }
                HostCommand::ProvideSourcePage { request_id, page } => self.source_page(
                    window,
                    binding.clone(),
                    update.generation,
                    *request_id,
                    (**page).clone(),
                ),
                HostCommand::ResolveStream { request_id, media } => {
                    if !same_media {
                        Err(ErrorCode::StaleHandle)
                    } else {
                        self.playback(window, binding.clone(), update.clone(), *request_id, media)
                    }
                }
                HostCommand::SourceError {
                    request_id,
                    message,
                } => {
                    let Some(state) = self.active_window(window) else {
                        return;
                    };
                    let result = state.inbox.lock().requests.fail(
                        &binding.installed.installation_id,
                        *request_id,
                        update.generation.context,
                    );
                    if result.is_ok() {
                        self.error(
                            window,
                            Some(plugin.into()),
                            Some(*request_id),
                            ErrorCode::IoError,
                            Some(message.clone()),
                        );
                    }
                    result
                }
                _ => {
                    if same_media || is_slot_command(command) {
                        indices.push(index);
                    }
                    Ok(())
                }
            };
            if let Err(error) = result {
                if matches!(
                    error,
                    ErrorCode::Denied | ErrorCode::InvalidArgument | ErrorCode::BadState
                ) {
                    self.fail_plugin(plugin, error);
                    return;
                }
                // Late/cancelled source responses are discarded, not allowed
                // to replace a newer view or to start unsolicited playback.
                if !matches!(error, ErrorCode::StaleHandle | ErrorCode::Cancelled) {
                    if let HostCommand::RequestOrigin { request_id, origin } = command {
                        let notified = self
                            .plugins
                            .get(plugin)
                            .and_then(|p| p.control.as_ref())
                            .is_some_and(|control| {
                                control
                                    .send(
                                        window,
                                        HostEvent::OriginGranted {
                                            request_id: *request_id,
                                            origin: origin.clone(),
                                            grant: None,
                                            insecure_auth: false,
                                        },
                                    )
                                    .is_ok()
                            });
                        self.error(
                            window,
                            Some(plugin.into()),
                            (!notified).then_some(*request_id),
                            error,
                            None,
                        );
                    } else {
                        let request = match command {
                            HostCommand::ProvideSourcePage { request_id, .. }
                            | HostCommand::ResolveStream { request_id, .. } => Some(*request_id),
                            _ => None,
                        };
                        self.error(window, Some(plugin.into()), request, error, None);
                    }
                }
            }
        }
        if !indices.is_empty()
            && let Some(win) = self.active_window(window)
            && win
                .inbox
                .push(Notice::Commands(Commands {
                    binding,
                    update,
                    indices,
                }))
                .is_err()
        {
            self.fail_plugin(plugin, ErrorCode::Backpressure);
        }
    }

    fn source_page(
        &self,
        window: Id,
        binding: Arc<Binding>,
        generation: Generation,
        request: Id,
        page: iris_api::v2::source::SourcePage,
    ) -> Result<(), ErrorCode> {
        let inbox = &self
            .active_window(window)
            .ok_or(ErrorCode::StaleHandle)?
            .inbox;
        let page = inbox.lock().requests.accept_page(
            &binding.installed.installation_id,
            request,
            generation.context,
            page,
        )?;
        inbox.push(Notice::Page {
            binding,
            generation,
            request,
            page: Arc::new(page),
        })
    }

    fn playback(
        &self,
        window: Id,
        binding: Arc<Binding>,
        update: Arc<Update>,
        request: Id,
        media: &iris_api::v2::source::ResolvedMedia,
    ) -> Result<(), ErrorCode> {
        let inbox = self
            .active_window(window)
            .ok_or(ErrorCode::StaleHandle)?
            .inbox
            .clone();
        let owner = binding.installed.installation_id.clone();
        let ticket = inbox
            .lock()
            .requests
            .take(&owner, request, update.generation.context)?;
        if !matches!(
            ticket.target,
            super::super::requests::RequestTarget::Resolve { .. }
                | super::super::requests::RequestTarget::Url { .. }
        ) {
            return Err(ErrorCode::Denied);
        }
        let control = self.plugins[&binding.installed.plugin_id]
            .control
            .as_ref()
            .ok_or(ErrorCode::StaleHandle)?;
        let permit = control.playback_permit(window, update.generation)?;
        let memory = self.shared.ledger.reserve(
            &owner,
            &[(Resource::HostBytes, 2 << 20), (Resource::Task, 1)],
        )?;
        let media = media.clone();
        let shared = Arc::downgrade(&self.shared);
        self.config.runtime.resources.io.submit(
            Role::Stream,
            &owner,
            Box::new(move || {
                let result =
                    guarded(|| permit.prepare(&media.stream, &update.blobs, &ticket.cancel));
                if !binding.is_live()
                    || ticket.cancel.is_cancelled()
                    || inbox.generation() != update.generation
                {
                    return;
                }
                let notice = match result {
                    Ok(playback) => Notice::Playback {
                        binding,
                        generation: update.generation,
                        ticket,
                        playback,
                        playlist: media.playlist,
                        _memory: memory,
                    },
                    Err(code) => Notice::Error {
                        plugin: Some(binding.installed.plugin_id.clone()),
                        request: Some(request),
                        code,
                        message: None,
                    },
                };
                let id = binding_id(&notice);
                if inbox.push(notice) == Err(ErrorCode::Backpressure)
                    && let Some(id) = id
                {
                    inbox.lock().faults.insert(id, ErrorCode::Backpressure);
                    if let Some(shared) = shared.upgrade() {
                        shared.signal();
                    }
                }
            }),
        )
    }
}

fn binding_id(notice: &Notice) -> Option<String> {
    match notice {
        Notice::Playback { binding, .. } => Some(binding.installed.plugin_id.clone()),
        Notice::Error { plugin, .. } => plugin.clone(),
        _ => None,
    }
}

pub(super) fn is_slot_command(command: &HostCommand) -> bool {
    matches!(
        command,
        HostCommand::SetSidebar { .. }
            | HostCommand::SetStatus { .. }
            | HostCommand::SetToolbar { .. }
            | HostCommand::SetOverlay { .. }
            | HostCommand::OpenModal { .. }
            | HostCommand::CloseModal { .. }
            | HostCommand::SetTheme { .. }
    )
}
