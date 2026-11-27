use super::{
    Binding, Catalog, Config, Inbox, Notice, PluginInfo, PluginStatus, Shared, SourceSnapshot,
    jobs::{Completed, Work},
};
use crate::{
    actor::Status,
    installation::{Installed, Review},
    ledger::Resource,
    pool::WorkPool,
    runtime::Control,
};
use iris_api::v2::{ErrorCode, Generation, Id, source::PlaybackSnapshot};
use iris_net::concurrency::{CancelToken, Deadline};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub(super) struct Plugin {
    pub record: Result<Installed, ErrorCode>,
    pub enabled: bool,
    pub status: PluginStatus,
    pub failure: Option<ErrorCode>,
    pub binding: Option<Arc<Binding>>,
    pub sources: Option<Arc<SourceSnapshot>>,
    pub control: Option<Control>,
    pub loading: Option<(Id, CancelToken)>,
    pub restart: bool,
    pub attached: Option<Id>,
    pub preparing: Option<Id>,
}
impl Plugin {
    pub fn stop(&mut self) {
        if let Some((_, cancel)) = &self.loading {
            cancel.cancel();
        }
        if let Some(binding) = &self.binding {
            binding.alive.store(false, Ordering::Release);
        }
        if let Some(control) = &mut self.control {
            control.stop();
        }
        self.attached = None;
        self.preparing = None;
        self.sources = None;
    }
    pub fn wants_control_reap(&self, has_window: bool) -> bool {
        let Some(control) = &self.control else {
            return false;
        };
        !matches!(control.status(), Status::Ready | Status::Initializing)
            && (matches!(control.status(), Status::Stopping | Status::Stopped)
                || self.binding.as_ref().is_none_or(|b| !b.is_live())
                || !matches!(self.status, PluginStatus::Failed(_))
                || self.restart
                || !self.enabled
                || !has_window)
    }
}
pub(super) struct Window {
    pub inbox: Arc<Inbox>,
    pub snapshot: Option<PlaybackSnapshot>,
    pub generation: Generation,
}
pub(super) struct PendingReview {
    pub window: Id,
    pub cancel: CancelToken,
    pub expires: Deadline,
    pub review: Option<Review>,
}
pub(super) struct Worker {
    pub shared: Arc<Shared>,
    pub config: Config,
    pub load_pool: WorkPool,
    pub disk_pool: WorkPool,
    pub plugins: BTreeMap<String, Plugin>,
    pub window: Option<Window>,
    pub reviews: BTreeMap<Id, PendingReview>,
    pub review_slots: BTreeSet<Id>,
    pub official_reviews: BTreeMap<String, Review>,
    pub origins: BTreeMap<Id, super::origin::PendingOrigin>,
    pub origin_slots: BTreeSet<Id>,
    pub retiring: Vec<Control>,
    pub uninstalling: BTreeSet<String>,
    pub scanning: bool,
    pub rescan: bool,
    pub revision: u64,
    pub dirty: bool,
    pub shutdown: bool,
    pub jobs: usize,
}
impl Worker {
    pub fn new(shared: Arc<Shared>, config: Config) -> Result<Self, ErrorCode> {
        Ok(Self {
            shared,
            config,
            load_pool: WorkPool::new(
                "package",
                iris_net::concurrency::budget::PLUGIN_PACKAGE_IO_WORKERS,
                4,
            )?,
            disk_pool: WorkPool::new(
                "setup",
                iris_net::concurrency::budget::PLUGIN_SETUP_IO_WORKERS,
                8,
            )?,
            plugins: BTreeMap::new(),
            window: None,
            reviews: BTreeMap::new(),
            review_slots: BTreeSet::new(),
            official_reviews: BTreeMap::new(),
            origins: BTreeMap::new(),
            origin_slots: BTreeSet::new(),
            retiring: Vec::new(),
            uninstalling: BTreeSet::new(),
            scanning: false,
            rescan: true,
            revision: 0,
            dirty: false,
            shutdown: false,
            jobs: 0,
        })
    }

    pub fn active_window(&self, id: Id) -> Option<&Window> {
        self.window.as_ref().filter(|w| w.inbox.id == id)
    }

    pub fn active_window_mut(&mut self, id: Id) -> Option<&mut Window> {
        self.window.as_mut().filter(|w| w.inbox.id == id)
    }

    pub fn run(mut self) {
        loop {
            let (commands, completed, shutdown) = {
                let mut gate = self.shared.gate.lock().unwrap_or_else(|e| e.into_inner());
                gate.wake = false;
                (
                    std::mem::take(&mut gate.commands),
                    std::mem::take(&mut gate.completed),
                    gate.shutdown,
                )
            };
            self.shutdown |= shutdown;
            self.close_windows();
            self.expire_reviews();
            self.expire_origins();
            for command in commands {
                self.command(command);
            }
            for Completed { work, _lease } in completed {
                self.jobs -= 1;
                self.completed(work);
            }
            self.cancellations();
            self.expire_reviews();
            self.expire_origins();
            self.poll_plugins();
            if !self.shutdown && self.window.is_some() {
                self.pump();
            }
            self.publish();
            if self.shutdown
                && self.jobs == 0
                && self.retiring.is_empty()
                && self.plugins.values().all(|p| p.control.is_none())
            {
                return;
            }
            let has_window = self.window.is_some();
            let cleaning = !self.retiring.is_empty()
                || self.plugins.values().any(|p| {
                    p.wants_control_reap(has_window)
                        && p.control.as_ref().is_some_and(|c| !c.is_quiescent())
                });
            let pending_attach = !self.shutdown
                && self.window.as_ref().is_some_and(|win| {
                    self.plugins.values().any(|p| {
                        p.enabled
                            && p.control.as_ref().is_some_and(|c| {
                                matches!(c.status(), Status::Ready | Status::Initializing)
                            })
                            && p.attached != Some(win.inbox.id)
                            && p.preparing != Some(win.inbox.id)
                    })
                });
            let timeout = if cleaning || pending_attach {
                Some(Duration::from_millis(100))
            } else {
                self.reviews
                    .values()
                    .map(|r| {
                        r.expires
                            .instant()
                            .saturating_duration_since(Instant::now())
                    })
                    .chain(
                        self.origins
                            .values()
                            .map(|r| r.expires.saturating_duration_since(Instant::now())),
                    )
                    .min()
            };
            let mut gate = self.shared.gate.lock().unwrap_or_else(|e| e.into_inner());
            if !gate.wake && gate.commands.is_empty() && gate.completed.is_empty() {
                gate = if let Some(timeout) = timeout {
                    self.shared
                        .changed
                        .wait_timeout(gate, timeout)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    self.shared
                        .changed
                        .wait(gate)
                        .unwrap_or_else(|e| e.into_inner())
                };
            }
            drop(gate);
        }
    }

    pub(super) fn close_active_window(&mut self) {
        if let Some(window) = self.window.take() {
            let id = window.inbox.id;
            {
                let mut state = window.inbox.lock();
                state.closed = true;
                for plugin in self.plugins.values() {
                    if let Ok(record) = &plugin.record {
                        state.requests.cancel_owner(&record.installation_id);
                    }
                }
            }
            for plugin in self.plugins.values_mut() {
                if plugin
                    .attached
                    .take_if(|attached| *attached == id)
                    .is_some()
                    && let Some(control) = &mut plugin.control
                {
                    let _ = control.close_window(id);
                }
                if plugin.preparing == Some(id) {
                    plugin.preparing = None;
                }
            }
        }
    }

    fn close_windows(&mut self) {
        if self
            .window
            .as_ref()
            .is_some_and(|w| self.shutdown || w.inbox.is_closed())
        {
            self.close_active_window();
        }
        if self.window.is_none() || self.shutdown {
            for plugin in self.plugins.values_mut() {
                plugin.stop();
                // Reload after the next window attaches, not while dormant.
                plugin.restart = plugin.enabled;
            }
        }
    }

    fn expire_reviews(&mut self) {
        let active_id = self.window.as_ref().map(|w| w.inbox.id);
        let stale: Vec<_> = self
            .reviews
            .iter()
            .filter_map(|(id, r)| {
                (r.expires.is_expired() || active_id != Some(r.window)).then_some(*id)
            })
            .collect();
        for id in stale {
            if let Some(review) = self.reviews.remove(&id) {
                let expired = review.expires.is_expired();
                let window_gone = active_id != Some(review.window);
                if review.review.is_some() {
                    self.review_slots.remove(&id);
                }
                review.cancel.cancel();
                if window_gone
                    && !expired
                    && let Some(inner) = review.review
                    && inner.is_bundled()
                {
                    self.official_reviews
                        .insert(inner.manifest().id.clone(), inner);
                } else {
                    self.error(review.window, None, Some(id), ErrorCode::StaleHandle, None);
                }
            }
        }
    }

    fn poll_plugins(&mut self) {
        self.retiring.retain(|c| !c.is_quiescent());
        let ids: Vec<_> = self.plugins.keys().cloned().collect();
        for id in ids {
            let mut updates = Vec::new();
            if let Some(control) = &mut self.plugins.get_mut(&id).unwrap().control {
                for _ in 0..8 {
                    if let Some(update) = control.poll() {
                        updates.push(update);
                    } else {
                        break;
                    }
                }
            }
            if updates.len() == 8 {
                self.shared.signal();
            }
            for update in updates {
                self.deliver(&id, update);
            }
            let source_result = self.refresh_sources(&id);
            if let Err(error) = source_result {
                self.fail_plugin(&id, error);
            }
            if let Some(Status::Failed(error)) =
                self.plugins[&id].control.as_ref().map(Control::status)
            {
                if matches!(
                    error,
                    ErrorCode::Denied | ErrorCode::InvalidArgument | ErrorCode::BadState
                ) {
                    self.fail_plugin(&id, error);
                } else {
                    self.fail_control(&id, error);
                }
            }
            let uninstalling = self.uninstalling.contains(&id);
            let plugin = self.plugins.get_mut(&id).unwrap();
            let status = if uninstalling {
                PluginStatus::Stopping
            } else if !plugin.enabled {
                PluginStatus::Disabled
            } else if let Some(error) = plugin.failure {
                PluginStatus::Failed(error)
            } else if let Some(control) = &plugin.control {
                match control.status() {
                    Status::Initializing => PluginStatus::Loading,
                    Status::Ready => PluginStatus::Ready,
                    Status::Stopping => PluginStatus::Stopping,
                    Status::Stopped => PluginStatus::Dormant,
                    Status::Failed(error) => PluginStatus::Failed(error),
                }
            } else if plugin.loading.is_some() {
                PluginStatus::Loading
            } else if plugin.restart || self.window.is_none() {
                PluginStatus::Dormant
            } else {
                plugin.status
            };
            if status != plugin.status {
                plugin.status = status;
                self.dirty = true;
            }
            // An ordinary control trap may leave authorized independent
            // media alive. Keep that Control until an explicit stop/reload.
            if plugin.wants_control_reap(self.window.is_some())
                && plugin.control.as_ref().is_some_and(Control::is_quiescent)
            {
                plugin.control.take();
                self.dirty = true;
            }
        }
    }

    fn pump(&mut self) {
        if self.rescan && !self.scanning {
            let installer = self.config.installer.clone();
            let bootstrap = self.config.bootstrap.clone();
            let setup = self.config.runtime.clone();
            if self
                .submit(false, "scan", move || {
                    Work::Scanned(super::jobs::guarded(|| match bootstrap {
                        Some(bootstrap) => bootstrap.scan(installer, &setup, &CancelToken::new()),
                        None => installer.entries().map(|entries| {
                            super::super::official::BootstrapScan {
                                entries,
                                pending_reviews: Vec::new(),
                            }
                        }),
                    }))
                })
                .is_ok()
            {
                self.scanning = true;
                self.rescan = false;
            }
        }
        if !self.official_reviews.is_empty() {
            self.surface_official_reviews();
        }
        let Some(window_id) = self.window.as_ref().map(|w| w.inbox.id) else {
            return;
        };
        let ids: Vec<_> = self.plugins.keys().cloned().collect();
        for id in ids {
            let plugin = &self.plugins[&id];
            if !plugin.enabled {
                continue;
            }
            if plugin.control.is_none()
                && plugin.loading.is_none()
                && plugin.restart
                && self
                    .plugins
                    .values()
                    .filter(|p| p.loading.is_some())
                    .count()
                    < 2
            {
                self.start_load(&id);
            }
            if self.plugins[&id]
                .control
                .as_ref()
                .is_some_and(|c| matches!(c.status(), Status::Ready | Status::Initializing))
            {
                self.prepare_window(&id, window_id);
            }
        }
    }

    pub fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.revision = self.revision.saturating_add(1);
        let catalog = Arc::new(Catalog {
            revision: self.revision,
            plugins: self
                .plugins
                .iter()
                .map(|(id, plugin)| PluginInfo {
                    id: id.clone(),
                    enabled: plugin.enabled,
                    status: plugin.status,
                    binding: plugin.binding.clone(),
                    sources: plugin.sources.clone(),
                })
                .collect(),
        });
        *self
            .shared
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = catalog;
        if let Some(window) = &self.window {
            let allowed: BTreeSet<_> = self
                .plugins
                .values()
                .filter(|p| {
                    p.enabled
                        && !matches!(
                            p.status,
                            PluginStatus::Failed(_)
                                | PluginStatus::Stopping
                                | PluginStatus::Disabled
                        )
                        && p.attached == Some(window.inbox.id)
                })
                .filter_map(|p| {
                    p.binding
                        .as_ref()
                        .filter(|b| b.is_live())
                        .map(|b| b.installed.installation_id.as_str())
                })
                .collect();
            let should_wake = {
                let mut state = window.inbox.lock();
                state.ready.retain(|id| allowed.contains(id.as_str()));
                state.access.retain(|id, _| allowed.contains(id.as_str()));
                if state.closed || state.wake_pending {
                    false
                } else {
                    state.wake_pending = true;
                    true
                }
            };
            if should_wake {
                let _ = window.inbox.wake.unbounded_send(());
            }
        }
    }

    pub fn error(
        &self,
        window: Id,
        plugin: Option<String>,
        request: Option<Id>,
        code: ErrorCode,
        message: Option<String>,
    ) {
        if let Some(window) = self.active_window(window) {
            let _ = window.inbox.push(Notice::Error {
                plugin,
                request,
                code,
                message,
            });
        }
    }

    pub fn fail_plugin(&mut self, id: &str, error: ErrorCode) {
        let mut installation_id = None;
        if let Some(plugin) = self.plugins.get_mut(id) {
            plugin.stop();
            plugin.restart = false;
            plugin.status = PluginStatus::Failed(error);
            plugin.failure = Some(error);
            self.dirty = true;
            if let Ok(record) = &plugin.record {
                installation_id = Some(record.installation_id.clone());
            }
        }
        if let Some(installation_id) = installation_id {
            self.fail_plugin_requests(id, &installation_id, error);
        }
    }

    /// Marks a plugin's control actor failed without revoking its `Binding` or
    /// stopping already-admitted `Playback` relays, while cancelling stranded
    /// `SourceRequests` and clearing window readiness.
    fn fail_control(&mut self, id: &str, error: ErrorCode) {
        let mut installation_id = None;
        if let Some(plugin) = self.plugins.get_mut(id) {
            if plugin.failure.is_some() {
                return;
            }
            plugin.restart = false;
            plugin.status = PluginStatus::Failed(error);
            plugin.failure = Some(error);
            plugin.preparing = None;
            plugin.sources = None;
            self.dirty = true;
            if let Ok(record) = &plugin.record {
                installation_id = Some(record.installation_id.clone());
            }
        }
        if let Some(installation_id) = installation_id {
            self.fail_plugin_requests(id, &installation_id, error);
        }
    }

    fn fail_plugin_requests(&self, id: &str, installation_id: &str, error: ErrorCode) {
        let Some(window) = &self.window else {
            return;
        };
        let window_id = window.inbox.id;
        let ids = {
            let mut state = window.inbox.lock();
            let ready_playback_ids: BTreeSet<Id> = state
                .notices
                .iter()
                .filter_map(|n| match n {
                    Notice::Playback { ticket, .. } => Some(ticket.id),
                    _ => None,
                })
                .collect();
            state
                .requests
                .cancel_owner(installation_id)
                .into_iter()
                .filter(|req_id| !ready_playback_ids.contains(req_id))
                .collect::<Vec<_>>()
        };
        for req_id in ids {
            self.error(window_id, Some(id.to_owned()), Some(req_id), error, None);
        }
    }

    fn refresh_sources(&mut self, id: &str) -> Result<(), ErrorCode> {
        let plugin = self.plugins.get_mut(id).unwrap();
        let Some(control) = &plugin.control else {
            return Ok(());
        };
        if plugin.failure.is_some() || !plugin.binding.as_ref().is_some_and(|b| b.is_live()) {
            return Ok(());
        }
        let sources = control.sources();
        if plugin
            .sources
            .as_ref()
            .is_some_and(|s| s.sources == sources)
        {
            return Ok(());
        }
        let bytes = estimate_sources_bytes(sources)?;
        let lease = self.shared.ledger.reserve(
            &control.installed().installation_id,
            &[(Resource::HostBytes, bytes * 32 + 4096)],
        )?;
        plugin.sources = Some(Arc::new(SourceSnapshot {
            sources: sources.to_vec(),
            _lease: lease,
        }));
        self.dirty = true;
        Ok(())
    }
}

fn estimate_sources_bytes(
    sources: &[iris_api::v2::source::SourceDescriptor],
) -> Result<u64, ErrorCode> {
    let mut total: usize = 2;
    for s in sources {
        total = total
            .saturating_add(160)
            .saturating_add(s.id.len())
            .saturating_add(s.name.len())
            .saturating_add(s.subtitle.as_ref().map_or(0, String::len))
            .saturating_add(s.icon.as_ref().map_or(0, String::len))
            .saturating_add(s.badge.as_ref().map_or(0, String::len))
            .saturating_add(s.auth_command_id.as_ref().map_or(0, String::len));
        if total > iris_api::v2::MESSAGE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    Ok(total as u64)
}
