use super::{
    Binding, Catalog, Config, Inbox, Notice, PluginInfo, PluginStatus, Shared, SourceSnapshot,
    jobs::{Completed, Work},
};
use crate::plugin_v2::{
    actor::Status,
    installation::{Installed, Review},
    ledger::Resource,
    pool::WorkPool,
    runtime::Control,
};
use iris_api::v2::{ErrorCode, Generation, Id, source::PlaybackSnapshot};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    pub loading: Option<(Id, Arc<AtomicBool>)>,
    pub restart: bool,
    pub attached: BTreeSet<Id>,
    pub preparing: BTreeSet<Id>,
}
impl Plugin {
    pub fn stop(&mut self) {
        if let Some((_, cancel)) = &self.loading {
            cancel.store(true, Ordering::Release);
        }
        if let Some(binding) = &self.binding {
            binding.alive.store(false, Ordering::Release);
        }
        if let Some(control) = &mut self.control {
            control.stop();
        }
        self.attached.clear();
        self.preparing.clear();
        self.sources = None;
    }
}
pub(super) struct Window {
    pub inbox: Arc<Inbox>,
    pub snapshot: Option<PlaybackSnapshot>,
    pub generation: Generation,
}
pub(super) struct PendingReview {
    pub window: Id,
    pub cancel: Arc<AtomicBool>,
    pub expires: Instant,
    pub review: Option<Review>,
}
pub(super) struct Worker {
    pub shared: Arc<Shared>,
    pub config: Config,
    pub load_pool: WorkPool,
    pub disk_pool: WorkPool,
    pub plugins: BTreeMap<String, Plugin>,
    pub windows: BTreeMap<Id, Window>,
    pub reviews: BTreeMap<Id, PendingReview>,
    pub review_slots: BTreeSet<Id>,
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
            load_pool: WorkPool::new("package", 2, 4)?,
            disk_pool: WorkPool::new("setup", 2, 8)?,
            plugins: BTreeMap::new(),
            windows: BTreeMap::new(),
            reviews: BTreeMap::new(),
            review_slots: BTreeSet::new(),
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
            if !self.shutdown && !self.windows.is_empty() {
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
            let cleaning = !self.retiring.is_empty()
                || self.plugins.values().any(|p| {
                    p.control.as_ref().is_some_and(|c| {
                        !c.is_quiescent()
                            && matches!(
                                c.status(),
                                Status::Stopping | Status::Stopped | Status::Failed(_)
                            )
                    })
                });
            let pending_attach = !self.shutdown
                && self.plugins.values().any(|p| {
                    p.enabled
                        && p.control.as_ref().is_some_and(|c| {
                            matches!(c.status(), Status::Ready | Status::Initializing)
                        })
                        && self
                            .windows
                            .keys()
                            .any(|w| !p.attached.contains(w) && !p.preparing.contains(w))
                });
            let timeout = if cleaning || pending_attach {
                Some(Duration::from_millis(100))
            } else {
                self.reviews
                    .values()
                    .map(|r| r.expires.saturating_duration_since(Instant::now()))
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
    fn close_windows(&mut self) {
        let closed: Vec<_> = self
            .windows
            .iter()
            .filter_map(|(id, w)| {
                (self.shutdown || w.inbox.closed.load(Ordering::Acquire)).then_some(*id)
            })
            .collect();
        for id in closed {
            if let Some(window) = self.windows.remove(&id) {
                window.inbox.closed.store(true, Ordering::Release);
                let mut requests = window
                    .inbox
                    .requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for plugin in self.plugins.values_mut() {
                    if let Ok(record) = &plugin.record {
                        requests.cancel_owner(&record.installation_id);
                    }
                    if plugin.attached.remove(&id)
                        && let Some(control) = &mut plugin.control
                    {
                        let _ = control.close_window(id);
                    }
                    plugin.preparing.remove(&id);
                }
            }
        }
        if self.windows.is_empty() || self.shutdown {
            for plugin in self.plugins.values_mut() {
                plugin.stop();
                // Reload after the next window attaches, not while dormant.
                plugin.restart = plugin.enabled;
            }
        }
    }
    fn expire_reviews(&mut self) {
        let stale: Vec<_> = self
            .reviews
            .iter()
            .filter_map(|(id, r)| {
                (r.expires <= Instant::now() || !self.windows.contains_key(&r.window))
                    .then_some(*id)
            })
            .collect();
        for id in stale {
            if let Some(review) = self.reviews.remove(&id) {
                if review.review.is_some() {
                    self.review_slots.remove(&id);
                }
                review.cancel.store(true, Ordering::Release);
                self.error(review.window, None, Some(id), ErrorCode::StaleHandle, None);
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
            if let Some(Status::Failed(
                error @ (ErrorCode::Denied | ErrorCode::InvalidArgument | ErrorCode::BadState),
            )) = self.plugins[&id].control.as_ref().map(Control::status)
            {
                self.fail_plugin(&id, error);
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
            } else if plugin.restart || self.windows.is_empty() {
                PluginStatus::Dormant
            } else {
                plugin.status
            };
            if status != plugin.status {
                plugin.status = status;
                self.dirty = true;
            }
            if plugin.control.as_ref().is_some_and(|c| {
                !matches!(c.status(), Status::Ready | Status::Initializing) && c.is_quiescent()
            }) {
                // An ordinary control trap may leave authorized independent
                // media alive. Keep that Control until an explicit stop/reload.
                if plugin
                    .control
                    .as_ref()
                    .is_some_and(|c| matches!(c.status(), Status::Stopping | Status::Stopped))
                    || plugin.binding.as_ref().is_none_or(|b| !b.is_live())
                    || !matches!(plugin.status, PluginStatus::Failed(_))
                    || plugin.restart
                    || !plugin.enabled
                    || self.windows.is_empty()
                {
                    plugin.control.take();
                    self.dirty = true;
                }
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
                        Some(bootstrap) => bootstrap.entries(installer, &setup),
                        None => installer.entries(),
                    }))
                })
                .is_ok()
            {
                self.scanning = true;
                self.rescan = false;
            }
        }
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
                let windows: Vec<_> = self.windows.keys().copied().collect();
                for window in windows {
                    self.prepare_window(&id, window);
                }
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
        for window in self.windows.values() {
            let allowed: BTreeSet<_> = self
                .plugins
                .values()
                .filter(|p| p.attached.contains(&window.inbox.id))
                .filter_map(|p| {
                    p.binding
                        .as_ref()
                        .filter(|b| b.is_live())
                        .map(|b| b.installed.installation_id.as_str())
                })
                .collect();
            window
                .inbox
                .ready
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id| allowed.contains(id.as_str()));
            window
                .inbox
                .access
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id, _| allowed.contains(id.as_str()));
            window.inbox.notify();
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
        if let Some(window) = self.windows.get(&window) {
            let _ = window.inbox.push(Notice::Error {
                plugin,
                request,
                code,
                message,
            });
        }
    }
    pub fn fail_plugin(&mut self, id: &str, error: ErrorCode) {
        if let Some(plugin) = self.plugins.get_mut(id) {
            plugin.stop();
            plugin.restart = false;
            plugin.status = PluginStatus::Failed(error);
            plugin.failure = Some(error);
            self.dirty = true;
            if let Ok(record) = &plugin.record {
                for window in self.windows.values() {
                    window
                        .inbox
                        .requests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .cancel_owner(&record.installation_id);
                }
            }
        }
    }
    fn refresh_sources(&mut self, id: &str) -> Result<(), ErrorCode> {
        let plugin = self.plugins.get_mut(id).unwrap();
        let Some(control) = &plugin.control else {
            return Ok(());
        };
        if !plugin.binding.as_ref().is_some_and(|b| b.is_live()) {
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
        let bytes = crate::plugin_v2::json::encode(&sources, iris_api::v2::MESSAGE_LIMIT)?;
        let lease = self.shared.ledger.reserve(
            &control.installed().installation_id,
            &[(Resource::HostBytes, bytes.len() as u64 * 32 + 4096)],
        )?;
        plugin.sources = Some(Arc::new(SourceSnapshot {
            sources: sources.to_vec(),
            _lease: lease,
        }));
        self.dirty = true;
        Ok(())
    }
}
