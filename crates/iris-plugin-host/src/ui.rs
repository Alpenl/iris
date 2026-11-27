//! Explicit [`WindowSession`] presentation and stream-lifecycle facade.
//! References keep the runtime's resource leases alive; polling is strictly
//! batch-bounded and never performs filesystem/network I/O or joins workers.
mod effects;
mod enhance;
mod overlay_rt;
mod slots;
mod snapshot;
mod stream_gate;
use super::{
    hub::{Binding, Catalog, Client, Notice, PluginInfo, PluginStatus},
    requests::BrowseTarget,
};
pub use effects::{
    ActionKind, EnhancementOutcome, EnhancementRequest, OverlayStep, POLL_ACTION_LIMIT,
    POLL_EFFECT_LIMIT, POLL_NOTICE_LIMIT, REMOTE_THUMB_LEASE_BYTES, REMOTE_THUMB_SCRATCH_BYTES,
    ReadyAnnotations, ReadyAttachment, ReadyEffect, ReadyMonitorLut, ReadyOverlayUpdate,
    ReadyStreamUpdate, RetainedThumbnail, RevokedEnhancements, TimelineLease, WindowAction,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, HostCommand, HostEvent, Id,
    source::{PlaybackSnapshot, SourceDescriptor},
};
use iris_api::{
    ChromeLayoutSpec, CommandSpec, OverlayNode, PluginModal, SidebarPanel, SourceProviderSpec,
    StatusBadge, ToolbarButton, UiThemeSpec,
};
use iris_core::player::PlaybackSnapshotInput;
pub use overlay_rt::ThumbnailReceiver;
pub use slots::DirtySlots;
pub use snapshot::snapshot_from_input;
pub type Event = HostEvent<serde_json::Value>;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

#[derive(Default)]
pub struct PollReport {
    pub actions: Vec<WindowAction>,
    pub effects: Vec<ReadyEffect>,
    pub notices: Vec<Notice>,
    pub dirty: DirtySlots,
    pub status_changed: bool,
}
impl PollReport {
    pub fn should_notify_ui(&self, fullscreen: bool, chrome: bool, sidebar: bool) -> bool {
        self.status_changed
            || !self.actions.is_empty()
            || !self.effects.is_empty()
            || !self.notices.is_empty()
            || self.dirty.should_notify_ui(fullscreen, chrome, sidebar)
    }
}

type UiOverrides = (Option<UiThemeSpec>, Option<ChromeLayoutSpec>);

/// Unified window-scoped plugin facade owning declarative view slots, active
/// [`crate::stream::session::StreamSession`] registration, and bounded [`ReadyEffect`] delivery.
pub struct WindowSession {
    client: Client,
    catalog: Arc<Catalog>,
    catalog_synced: bool,
    ready: BTreeSet<String>,
    slots: BTreeMap<String, slots::Slots>,
    modal_order: u64,
    selected_theme: Option<String>,
    overrides: RefCell<Option<UiOverrides>>,
    active_stream: Mutex<Option<stream_gate::ActiveStream>>,
    enhance: enhance::EnhancementState,
}

impl WindowSession {
    pub fn new(client: Client, selected_theme: Option<String>) -> Self {
        let catalog = client.catalog();
        let generation = client.generation();
        Self {
            client,
            catalog,
            catalog_synced: false,
            ready: BTreeSet::new(),
            slots: BTreeMap::new(),
            modal_order: 0,
            selected_theme,
            overrides: RefCell::new(None),
            active_stream: Mutex::new(None),
            enhance: enhance::EnhancementState::new(generation),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn is_connected(&self) -> bool {
        self.client.window_id() != Id(0)
    }

    pub fn is_ready(&self, plugin: &str) -> bool {
        self.client.is_ready(plugin)
    }

    pub fn generation(&self) -> Generation {
        self.client.generation()
    }

    pub fn list(&self) -> &[PluginInfo] {
        &self.catalog.plugins
    }

    /// Constructs a privacy-filtered [`PlaybackSnapshot`] for the current window media generation.
    pub fn playback_snapshot(
        &self,
        input: &PlaybackSnapshotInput,
        favorite: bool,
    ) -> PlaybackSnapshot {
        snapshot_from_input(input, favorite, self.generation().media)
    }

    /// Synchronizes the currently playing local file path across attached `FileExecutor`s.
    pub fn set_current_local_file(&self, path: Option<PathBuf>) {
        let media = self.generation().media;
        for info in self.list() {
            if let Ok(access) = self.client.access(&info.id) {
                let _ = access
                    .files
                    .set_current_path(media, access.generation, path.clone());
            }
        }
    }

    /// Prechecks whether `plugin_id` is enabled, ready, and granted `capability` before
    /// committing a media generation advance.
    pub fn precheck_capability(
        &self,
        plugin_id: &str,
        capability: Capability,
    ) -> Result<(), ErrorCode> {
        if self.generation().media.0.checked_add(1).is_none() {
            return Err(ErrorCode::QuotaExceeded);
        }
        if !self.is_connected() {
            return Err(ErrorCode::StaleHandle);
        }
        let Some(info) = self.list().iter().find(|p| p.id == plugin_id) else {
            return Err(ErrorCode::StaleHandle);
        };
        if !info.enabled {
            return Err(ErrorCode::StaleHandle);
        }
        match info.status {
            PluginStatus::Ready => {}
            PluginStatus::Failed(code) => return Err(code),
            PluginStatus::Loading | PluginStatus::Dormant => return Err(ErrorCode::Backpressure),
            PluginStatus::Disabled | PluginStatus::Stopping => return Err(ErrorCode::StaleHandle),
        }
        let Some(binding) = info.binding.as_ref().filter(|b| b.is_live()) else {
            return Err(ErrorCode::StaleHandle);
        };
        if !binding.installed.grants.contains(&capability) {
            return Err(ErrorCode::Denied);
        }
        if !self.client.is_ready(plugin_id) {
            return Err(ErrorCode::Backpressure);
        }
        Ok(())
    }

    /// Advances the window's media generation, atomically clearing any registered
    /// [`crate::stream::session::StreamSession`] and revoking prior-media enhancement state.
    pub fn advance_media(&mut self) -> Result<(), ErrorCode> {
        self.clear_active_stream();
        if !self.is_connected() {
            self.enhance.reset_for_media(self.generation());
            return Ok(());
        }
        let old = self.generation();
        let next_media = old.media.0.checked_add(1).ok_or(ErrorCode::QuotaExceeded)?;
        let next = Generation {
            media: Id(next_media),
            seek: Id(0),
            context: old.context,
        };
        self.client.advance(next)?;
        self.enhance.reset_for_media(self.generation());
        Ok(())
    }

    pub fn browse(&self, plugin: &str, target: BrowseTarget) -> Result<(Id, Id), ErrorCode> {
        self.client.browse(plugin, target)
    }

    pub fn next_page(&self, plugin: &str, view: Id, cursor: &str) -> Result<Id, ErrorCode> {
        self.client.next_page(plugin, view, cursor)
    }

    pub fn close_view(&self, plugin: &str, view: Id) -> Result<(), ErrorCode> {
        self.client.close_view(plugin, view)
    }

    pub fn resolve(
        &self,
        plugin: &str,
        source: &str,
        folder: Option<String>,
        entry: &str,
        variant: Option<String>,
    ) -> Result<Id, ErrorCode> {
        self.client.resolve(plugin, source, folder, entry, variant)
    }

    pub fn resolve_url(&self, plugin: &str, url: &str) -> Result<Id, ErrorCode> {
        self.client.resolve_url(plugin, url)
    }

    pub fn cancel_request(&self, plugin: &str, request: Id) -> Result<(), ErrorCode> {
        self.client.cancel_request(plugin, request)
    }

    pub fn review_package(&self, path: PathBuf) -> Result<Id, ErrorCode> {
        self.client.review(path)
    }

    pub fn approve_review(
        &self,
        id: Id,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    ) -> Result<(), ErrorCode> {
        self.client.approve(id, grants, keep_identity)
    }

    pub fn dismiss_review(&self, id: Id) -> Result<(), ErrorCode> {
        self.client.dismiss_review(id)
    }

    pub fn confirm_origin(
        &self,
        id: Id,
        allow: bool,
        insecure_auth: bool,
    ) -> Result<(), ErrorCode> {
        self.client.confirm_origin(id, allow, insecure_auth)
    }

    fn enabled(&self, id: &str) -> bool {
        self.catalog.plugins.iter().any(|p| {
            p.id == id
                && p.enabled
                && p.status == PluginStatus::Ready
                && p.binding.as_ref().is_some_and(|b| {
                    b.is_live() && self.ready.contains(&b.installed.installation_id)
                })
        })
    }

    pub(super) fn binding(&self, id: &str) -> Option<&Arc<Binding>> {
        self.catalog
            .plugins
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.binding.as_ref())
            .filter(|b| b.is_live())
    }

    /// Bounded, non-blocking poll draining at most [`POLL_NOTICE_LIMIT`] notices,
    /// [`POLL_ACTION_LIMIT`] guest commands, and [`POLL_EFFECT_LIMIT`] [`ReadyEffect`]s.
    pub fn poll(&mut self) -> PollReport {
        let mut report = PollReport::default();
        let ready = self.client.ready_installations();
        if ready != self.ready {
            self.ready = ready;
            report.status_changed = true;
            report.dirty.merge(DirtySlots::all());
        }
        let catalog = self.client.catalog();
        let needs_initial_seed = !self.catalog_synced
            && catalog
                .plugins
                .iter()
                .any(|p| p.status == PluginStatus::Ready);
        self.catalog_synced = true;
        if catalog.revision != self.catalog.revision || needs_initial_seed {
            let mut removed = Vec::new();
            for (id, slots) in &self.slots {
                let current = catalog.plugins.iter().find(|p| p.id == *id);
                if current.is_none_or(|p| {
                    !p.enabled
                        || p.status != PluginStatus::Ready
                        || p.binding
                            .as_ref()
                            .is_none_or(|b| !Arc::ptr_eq(&slots.binding, b) || !b.is_live())
                }) {
                    removed.push(id.clone());
                }
            }
            for id in removed {
                self.slots.remove(&id);
                self.clear_active_stream_for_plugin(&id);
                report.dirty.merge(DirtySlots::all());
            }
            for info in &catalog.plugins {
                if info.status == PluginStatus::Ready
                    && let Some(binding) = &info.binding
                    && binding.is_live()
                    && !self.slots.contains_key(&info.id)
                {
                    self.slots
                        .insert(info.id.clone(), slots::Slots::new(binding.clone()));
                    report.dirty.merge(DirtySlots::all());
                }
            }
            report.status_changed = true;
            report.dirty.sources = true;
            self.catalog = catalog;
        }
        let current_generation = self.generation();
        for notice in self
            .client
            .poll_bounded(POLL_NOTICE_LIMIT, POLL_ACTION_LIMIT)
        {
            let Notice::Commands(batch) = notice else {
                report.notices.push(notice);
                continue;
            };
            if !batch.binding.is_live() || batch.update.is_cancelled() {
                continue;
            }
            let id = &batch.binding.installed.plugin_id;
            let state = self
                .slots
                .entry(id.clone())
                .or_insert_with(|| slots::Slots::new(batch.binding.clone()));
            if !Arc::ptr_eq(&state.binding, &batch.binding) {
                continue;
            }
            for index in batch.indices {
                let command = &batch.update.commands[index];
                if let HostCommand::OpenModal { .. } = command {
                    self.modal_order = self.modal_order.saturating_add(1);
                }
                if !state.apply(&batch.update, index, self.modal_order, &mut report.dirty)
                    && let Some(action) =
                        effects::classify_command(&batch.binding, &batch.update, index)
                    && action.is_live(current_generation)
                {
                    report.actions.push(action);
                }
            }
        }
        report.effects = self.drain_ready_effects(current_generation);
        if report.dirty.theme {
            self.overrides.borrow_mut().take();
        }
        report
    }

    fn sent(plugin: Option<&str>, result: Result<(), ErrorCode>) -> PollReport {
        let mut report = PollReport::default();
        if let Err(code) = result {
            report.notices.push(Notice::Error {
                plugin: plugin.map(str::to_owned),
                request: None,
                code,
                message: None,
            });
        }
        report
    }

    pub fn send_to(&self, plugin: &str, event: HostEvent<serde_json::Value>) -> PollReport {
        Self::sent(Some(plugin), self.client.send(plugin, event))
    }

    pub fn broadcast(&self, event: &HostEvent<serde_json::Value>) -> PollReport {
        let mut report = PollReport::default();
        if let HostEvent::MediaOpened { snapshot, .. } | HostEvent::StateChanged { snapshot, .. } =
            event
        {
            report
                .notices
                .extend(Self::sent(None, self.client.update_snapshot(snapshot.clone())).notices);
        }
        for info in &self.catalog.plugins {
            if self.enabled(&info.id) {
                report.notices.extend(
                    Self::sent(Some(&info.id), self.client.send(&info.id, event.clone())).notices,
                );
            }
        }
        report
    }

    pub fn set_enabled_with_snapshot(
        &self,
        id: &str,
        enabled: bool,
        snapshot: PlaybackSnapshot,
    ) -> PollReport {
        if !enabled {
            self.clear_active_stream_for_plugin(id);
        }
        let result = self
            .client
            .update_snapshot(snapshot)
            .and_then(|_| self.client.set_enabled(id, enabled));
        Self::sent(Some(id), result)
    }

    pub fn uninstall(&self, id: &str, keep_data: bool) -> PollReport {
        self.clear_active_stream_for_plugin(id);
        Self::sent(Some(id), self.client.uninstall(id, keep_data))
    }

    pub fn reload_with_snapshot(
        &self,
        disabled: &[String],
        snapshot: PlaybackSnapshot,
    ) -> PollReport {
        self.clear_active_stream();
        let disabled: BTreeSet<String> = disabled.iter().cloned().collect();
        Self::sent(
            None,
            self.client
                .update_snapshot(snapshot)
                .and_then(|_| self.client.reload_with_disabled(disabled)),
        )
    }

    pub fn shutdown(&self) {
        if let Some(active) = self
            .active_stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            active.session.cancel();
        }
        self.enhance
            .ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown();
        self.client.shutdown();
    }

    pub fn has_state_listeners(&self) -> bool {
        self.catalog
            .plugins
            .iter()
            .filter(|p| self.enabled(&p.id))
            .any(|p| {
                p.binding.as_ref().is_some_and(|b| {
                    [
                        Capability::ReadState,
                        Capability::UiOverlay,
                        Capability::MediaEnhance,
                    ]
                    .iter()
                    .any(|cap| b.installed.grants.contains(cap))
                })
            })
    }

    pub fn source_providers(&self) -> Vec<(String, SourceProviderSpec)> {
        self.catalog
            .plugins
            .iter()
            .filter(|p| self.enabled(&p.id))
            .filter_map(|p| {
                let binding = p.binding.as_ref()?;
                if !binding.installed.grants.contains(&Capability::MediaSource) {
                    return None;
                }
                binding
                    .manifest
                    .source_provider
                    .clone()
                    .map(|provider| (p.id.clone(), provider))
            })
            .collect()
    }

    pub fn media_sources(&self) -> Vec<(&str, &str, &SourceDescriptor)> {
        self.catalog
            .plugins
            .iter()
            .filter(|p| self.enabled(&p.id))
            .flat_map(|p| {
                p.sources.iter().flat_map(|s| &s.sources).map(move |s| {
                    (
                        p.id.as_str(),
                        p.binding
                            .as_ref()
                            .map_or(p.id.as_str(), |b| b.manifest.name.as_str()),
                        s,
                    )
                })
            })
            .collect()
    }

    pub fn commands(&self) -> Vec<(&str, &str, &CommandSpec)> {
        self.catalog
            .plugins
            .iter()
            .filter(|p| self.enabled(&p.id))
            .flat_map(|p| {
                p.binding.iter().flat_map(move |b| {
                    b.manifest
                        .commands
                        .iter()
                        .map(move |c| (p.id.as_str(), b.manifest.name.as_str(), c))
                })
            })
            .collect()
    }

    pub fn url_resolvers(&self, url: Option<&str>) -> Vec<(String, String)> {
        self.catalog
            .plugins
            .iter()
            .filter(|p| self.enabled(&p.id))
            .filter_map(|p| {
                let binding = p.binding.as_ref()?;
                if !binding.installed.grants.contains(&Capability::UrlResolver)
                    || url.is_some_and(|url| {
                        !binding
                            .manifest
                            .url_patterns
                            .iter()
                            .any(|pattern| super::url_resolver::matches(pattern, url))
                    })
                {
                    return None;
                }
                Some((p.id.clone(), binding.manifest.name.clone()))
            })
            .collect()
    }

    pub fn sidebar_panels(&self) -> Vec<(&str, &SidebarPanel)> {
        self.slots
            .iter()
            .filter(|(id, _)| self.enabled(id))
            .filter_map(|(id, s)| s.sidebar().map(|p| (id.as_str(), p)))
            .collect()
    }

    pub fn status_badges(&self) -> Vec<(&str, &StatusBadge)> {
        self.slots
            .iter()
            .filter(|(id, _)| self.enabled(id))
            .filter_map(|(id, s)| s.status().map(|p| (id.as_str(), p)))
            .collect()
    }

    pub fn toolbar_buttons(&self) -> Vec<(&str, &ToolbarButton)> {
        self.slots
            .iter()
            .filter(|(id, _)| self.enabled(id))
            .flat_map(|(id, s)| s.toolbar().iter().map(move |p| (id.as_str(), p)))
            .collect()
    }

    pub fn overlay_nodes(&self) -> impl Iterator<Item = (&str, &OverlayNode)> {
        self.slots
            .iter()
            .filter(|(id, _)| self.enabled(id))
            .flat_map(|(id, s)| s.overlay().iter().map(move |p| (id.as_str(), p)))
    }

    pub fn active_modal(&self) -> Option<(&str, &PluginModal)> {
        self.slots
            .iter()
            .filter(|(id, _)| self.enabled(id))
            .filter_map(|(id, s)| s.modal().map(|m| (s.modal_order, id, m)))
            .max_by_key(|(order, _, _)| *order)
            .map(|(_, id, m)| (id.as_str(), m))
    }

    pub fn dismiss_modal(&mut self, id: &str) {
        if let Some(slot) = self.slots.get_mut(id) {
            slot.dismiss_modal();
        }
    }

    /// One explicit host selection controls theme/chrome. Disabled or failed
    /// plugins immediately fall back to native host appearance and escape keys.
    pub fn select_theme(&mut self, id: Option<String>) -> Result<(), ErrorCode> {
        if let Some(id) = &id {
            super::validation::identifier(id)?;
            if self
                .binding(id)
                .is_some_and(|b| !b.installed.grants.contains(&Capability::UiTheme))
            {
                return Err(ErrorCode::Denied);
            }
        }
        if self.selected_theme != id {
            self.selected_theme = id;
            self.overrides.borrow_mut().take();
        }
        Ok(())
    }

    pub fn active_ui_overrides(&self) -> UiOverrides {
        if self
            .selected_theme
            .as_ref()
            .is_none_or(|id| !self.enabled(id))
        {
            return (None, None);
        }
        if let Some(cached) = self.overrides.borrow().as_ref() {
            return cached.clone();
        }
        let value = self
            .selected_theme
            .as_ref()
            .filter(|id| self.enabled(id))
            .and_then(|id| self.slots.get(id))
            .map_or((None, None), |s| (s.theme().cloned(), s.chrome().cloned()));
        *self.overrides.borrow_mut() = Some(value.clone());
        value
    }
}

impl Drop for WindowSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests;
