use super::{
    Binding, Notice, PluginStatus, ReviewSummary,
    worker::{PendingReview, Plugin, Worker},
};
use crate::{
    concurrency::{CancelToken, Deadline},
    plugin_v2::{
        installation::{Installed, Review},
        ledger::{Reservation, Resource},
        runtime::{Control, PreparedWindow, Setup},
    },
};
use iris_api::v2::{ErrorCode, Generation, Id};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

pub(super) struct Completed {
    pub work: Work,
    pub _lease: Reservation,
}
pub(super) enum Work {
    OriginReviewed {
        id: Id,
        result: Result<Box<super::super::network::consent::OriginReview>, ErrorCode>,
    },
    OriginApplied {
        id: Id,
        result: Result<iris_api::v2::task_result::OriginResult, ErrorCode>,
    },
    Scanned(Result<super::super::official::BootstrapScan, ErrorCode>),
    Loaded {
        plugin: String,
        ticket: Id,
        result: Result<Box<Control>, ErrorCode>,
    },
    Window {
        plugin: String,
        window: Id,
        generation: Generation,
        result: Result<PreparedWindow, ErrorCode>,
    },
    Reviewed {
        id: Id,
        result: Result<Box<Review>, ErrorCode>,
    },
    Installed {
        window: Id,
        id: Id,
        result: Result<Installed, ErrorCode>,
    },
    Uninstalled {
        window: Id,
        plugin: String,
        result: Result<(), ErrorCode>,
    },
}
impl Worker {
    pub fn reserve_job_lease(&self) -> Result<Reservation, ErrorCode> {
        if self.jobs >= 8 {
            return Err(ErrorCode::Backpressure);
        }
        self.shared
            .ledger
            .reserve("host-coordinator", &[(Resource::HostBytes, 2 << 20)])
    }
    pub fn submit_reserved(
        &mut self,
        compile: bool,
        owner: &str,
        lease: Reservation,
        job: impl FnOnce() -> Work + Send + 'static,
    ) -> Result<(), ErrorCode> {
        let shared = self.shared.clone();
        let pool = if compile {
            &self.load_pool
        } else {
            &self.disk_pool
        };
        pool.submit(
            owner,
            Box::new(move || {
                let work = job();
                let mut gate = shared.gate.lock().unwrap_or_else(|e| e.into_inner());
                gate.completed.push_back(Completed {
                    work,
                    _lease: lease,
                });
                gate.wake = true;
                shared.changed.notify_one();
            }),
        )?;
        self.jobs += 1;
        Ok(())
    }
    pub fn submit(
        &mut self,
        compile: bool,
        owner: &str,
        job: impl FnOnce() -> Work + Send + 'static,
    ) -> Result<(), ErrorCode> {
        // Completion capacity is reserved at admission. No worker blocks on a
        // UI queue, and cancelled work stays charged until it really exits.
        let lease = self.reserve_job_lease()?;
        self.submit_reserved(compile, owner, lease, job)
    }
    pub fn start_load(&mut self, id: &str) {
        let Ok(installed) = self.plugins[id].record.clone() else {
            return;
        };
        let Ok(ticket) = super::super::instance::next_id().map(Id) else {
            return;
        };
        let cancel = CancelToken::new();
        let job_cancel = cancel.clone();
        let installer = self.config.installer.clone();
        let setup = self.config.runtime.clone();
        let plugin = id.to_owned();
        if self
            .submit(true, id, move || Work::Loaded {
                plugin,
                ticket,
                result: guarded(|| Control::load(&installer, installed, setup, &job_cancel))
                    .map(Box::new),
            })
            .is_ok()
        {
            let plugin = self.plugins.get_mut(id).unwrap();
            plugin.loading = Some((ticket, cancel));
            plugin.restart = false;
            plugin.status = PluginStatus::Loading;
            self.dirty = true;
        }
    }
    pub fn prepare_window(&mut self, plugin: &str, window: Id) {
        let state = &self.plugins[plugin];
        if state.attached.contains(&window) || state.preparing.contains(&window) {
            return;
        }
        let Some(binding) = state.binding.clone().filter(|b| b.is_live()) else {
            return;
        };
        let inbox = self.windows[&window].inbox.clone();
        let generation = self.windows[&window].generation;
        let snapshot = self.windows[&window].snapshot.clone();
        let browser_binding = binding.clone();
        let browser = Arc::new(move |transaction, generation, url: &str| {
            if !browser_binding.is_live() {
                return Err(ErrorCode::Cancelled);
            }
            inbox.push(Notice::Browser {
                binding: browser_binding.clone(),
                generation,
                transaction,
                url: url.into(),
            })
        });
        let plan = state
            .control
            .as_ref()
            .unwrap()
            .window_preparation(window, generation, snapshot, browser);
        let plan = match plan {
            Ok(plan) => plan,
            Err(ErrorCode::Backpressure | ErrorCode::QuotaExceeded) => return,
            Err(error) => {
                self.fail_plugin(plugin, error);
                return;
            }
        };
        let id = plugin.to_owned();
        if self
            .submit(false, &format!("window-{window:?}"), move || Work::Window {
                plugin: id,
                window,
                generation,
                result: guarded(|| plan.prepare()),
            })
            .is_ok()
        {
            self.plugins
                .get_mut(plugin)
                .unwrap()
                .preparing
                .insert(window);
        }
    }
    pub fn completed(&mut self, work: Work) {
        match work {
            Work::OriginReviewed { id, result } => self.origin_reviewed(id, result.map(|r| *r)),
            Work::OriginApplied { id, result } => self.origin_applied(id, result),
            Work::Scanned(result) => self.scanned(result),
            Work::Loaded {
                plugin,
                ticket,
                result,
            } => self.loaded(&plugin, ticket, result.map(|control| *control)),
            Work::Window {
                plugin,
                window,
                generation,
                result,
            } => {
                let Some(state) = self.plugins.get_mut(&plugin) else {
                    return;
                };
                state.preparing.remove(&window);
                if !state.enabled || !self.windows.contains_key(&window) {
                    return;
                }
                let Some(control) = &mut state.control else {
                    return;
                };
                let current = self.windows[&window].generation;
                if generation != current {
                    return;
                } // prepare again for the current snapshot
                match result.and_then(|prepared| control.attach_window(prepared)) {
                    Ok(()) => {
                        state.attached.insert(window);
                        let inbox = &self.windows[&window].inbox;
                        if let Ok(access) = control.window_access(window) {
                            inbox
                                .access
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(access.owner.clone(), access);
                        }
                        inbox
                            .ready
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(control.installed().installation_id.clone());
                        inbox.notify();
                    }
                    Err(
                        ErrorCode::StaleHandle
                        | ErrorCode::Cancelled
                        | ErrorCode::Backpressure
                        | ErrorCode::QuotaExceeded,
                    ) => {}
                    Err(error) => self.fail_plugin(&plugin, error),
                }
            }
            Work::Reviewed { id, result } => {
                let Some(pending) = self.reviews.get_mut(&id) else {
                    self.review_slots.remove(&id);
                    return;
                };
                let window = pending.window;
                match result {
                    Ok(review) => {
                        let summary = self
                            .shared
                            .ledger
                            .reserve("host-coordinator", &[(Resource::HostBytes, 2 << 20)])
                            .map(|lease| {
                                Arc::new(ReviewSummary {
                                    id,
                                    manifest: review.manifest().clone(),
                                    digest: review.digest(),
                                    previous: review.previous().cloned(),
                                    archived: review.archived().cloned(),
                                    _lease: lease,
                                })
                            });
                        pending.review = Some(*review);
                        match summary.and_then(|summary| {
                            self.windows
                                .get(&window)
                                .ok_or(ErrorCode::Cancelled)?
                                .inbox
                                .push(Notice::Review(summary))
                        }) {
                            Ok(()) => {}
                            Err(error) => {
                                self.reviews.remove(&id);
                                self.review_slots.remove(&id);
                                self.error(window, None, Some(id), error, None);
                            }
                        }
                    }
                    Err(error) => {
                        self.reviews.remove(&id);
                        self.review_slots.remove(&id);
                        self.error(window, None, Some(id), error, None);
                    }
                }
            }
            Work::Installed { window, id, result } => {
                self.review_slots.remove(&id);
                match result {
                    Ok(record) => {
                        if let Some(state) = self.plugins.get_mut(&record.plugin_id) {
                            state.stop();
                            state.record = Ok(record.clone());
                            state.enabled = true;
                            state.failure = None;
                            state.restart = true;
                        } else {
                            self.plugins.insert(
                                record.plugin_id.clone(),
                                Plugin {
                                    record: Ok(record.clone()),
                                    enabled: true,
                                    status: PluginStatus::Dormant,
                                    failure: None,
                                    binding: None,
                                    sources: None,
                                    control: None,
                                    loading: None,
                                    restart: true,
                                    attached: BTreeSet::new(),
                                    preparing: BTreeSet::new(),
                                },
                            );
                        }
                        self.config.disabled.remove(&record.plugin_id);
                        self.dirty = true;
                        if let Some(window) = self.windows.get(&window) {
                            let _ = window.inbox.push(Notice::Installed(record));
                        }
                    }
                    Err(error) => self.error(window, None, Some(id), error, None),
                }
            }
            Work::Uninstalled {
                window,
                plugin,
                result,
            } => {
                self.uninstalling.remove(&plugin);
                match result {
                    Ok(()) => {
                        if let Some(mut state) = self.plugins.remove(&plugin) {
                            state.stop();
                            if let Some(control) = state.control.take()
                                && !control.is_quiescent()
                            {
                                self.retiring.push(control);
                            }
                        }
                        self.config.disabled.remove(&plugin);
                        self.dirty = true;
                        if let Some(win) = self.windows.get(&window) {
                            let _ = win.inbox.push(Notice::Uninstalled(plugin));
                        }
                    }
                    Err(error) => {
                        self.rescan = true;
                        self.error(window, Some(plugin), None, error, None);
                    }
                }
            }
        }
    }
    fn scanned(&mut self, result: Result<super::super::official::BootstrapScan, ErrorCode>) {
        self.scanning = false;
        let scan = match result {
            Ok(scan) => scan,
            Err(error) => {
                for window in self.windows.keys() {
                    self.error(*window, None, None, error, None);
                }
                return;
            }
        };
        let entries = scan.entries;
        let seen: BTreeSet<_> = entries.iter().map(|e| e.plugin_id.clone()).collect();
        let removed_ids: Vec<_> = self
            .plugins
            .keys()
            .filter(|id| !seen.contains(*id) && !self.uninstalling.contains(*id))
            .cloned()
            .collect();
        for id in removed_ids {
            if let Some(mut plugin) = self.plugins.remove(&id) {
                plugin.stop();
                if let Some(control) = plugin.control.take()
                    && !control.is_quiescent()
                {
                    self.retiring.push(control);
                }
            }
        }
        for entry in entries {
            if self.uninstalling.contains(&entry.plugin_id) {
                continue;
            }
            let enabled = !self.config.disabled.contains(&entry.plugin_id);
            if let Some(plugin) = self.plugins.get_mut(&entry.plugin_id) {
                plugin.enabled = enabled;
                if plugin.record != entry.record || !enabled {
                    plugin.stop();
                    plugin.record = entry.record;
                }
                // Explicit reload is allowed to restart a failed module.
                plugin.restart = enabled;
                plugin.failure = plugin.record.as_ref().err().copied();
            } else {
                let status = entry
                    .record
                    .as_ref()
                    .map_or_else(|e| PluginStatus::Failed(*e), |_| PluginStatus::Dormant);
                self.plugins.insert(
                    entry.plugin_id,
                    Plugin {
                        failure: entry.record.as_ref().err().copied(),
                        record: entry.record,
                        enabled,
                        status,
                        binding: None,
                        sources: None,
                        control: None,
                        loading: None,
                        restart: enabled,
                        attached: BTreeSet::new(),
                        preparing: BTreeSet::new(),
                    },
                );
            }
        }
        for upgrade in scan.pending_reviews {
            let active = self.reviews.values().any(|pending| {
                pending
                    .review
                    .as_ref()
                    .is_some_and(|r| r.manifest().id == upgrade.plugin_id)
            });
            if !active {
                self.official_reviews
                    .insert(upgrade.plugin_id, upgrade.review);
            }
        }
        self.surface_official_reviews();
        self.dirty = true;
    }
    pub fn surface_official_reviews(&mut self) {
        let Some(&window) = self.windows.keys().next() else {
            return;
        };
        let keys: Vec<String> = self.official_reviews.keys().cloned().collect();
        for plugin_id in keys {
            if self.review_slots.len() >= 2 {
                break;
            }
            let Some(mut review) = self.official_reviews.remove(&plugin_id) else {
                continue;
            };
            let Ok(id) = super::super::instance::next_id().map(Id) else {
                self.official_reviews.insert(plugin_id, review);
                break;
            };
            let summary = self
                .shared
                .ledger
                .reserve("host-coordinator", &[(Resource::HostBytes, 2 << 20)])
                .map(|lease| {
                    Arc::new(ReviewSummary {
                        id,
                        manifest: review.manifest().clone(),
                        digest: review.digest(),
                        previous: review.previous().cloned(),
                        archived: review.archived().cloned(),
                        _lease: lease,
                    })
                });
            let pushed = summary.and_then(|summary| {
                self.windows
                    .get(&window)
                    .ok_or(ErrorCode::Cancelled)?
                    .inbox
                    .push(Notice::Review(summary))
            });
            if pushed.is_ok() {
                review.refresh_expiry();
                self.reviews.insert(
                    id,
                    PendingReview {
                        window,
                        cancel: CancelToken::new(),
                        expires: Deadline::after(Duration::from_secs(300)),
                        review: Some(review),
                    },
                );
                self.review_slots.insert(id);
            } else {
                self.official_reviews.insert(plugin_id, review);
                break;
            }
        }
    }
    fn loaded(&mut self, id: &str, ticket: Id, result: Result<Control, ErrorCode>) {
        let Some(state) = self.plugins.get_mut(id) else {
            return;
        };
        if !state.loading.as_ref().is_some_and(|(t, _)| *t == ticket) {
            return;
        }
        let (_, cancel) = state.loading.take().unwrap();
        match result {
            Ok(mut control) => {
                if cancel.is_cancelled()
                    || !state.enabled
                    || self.windows.is_empty()
                    || state.record.as_ref().ok() != Some(control.installed())
                {
                    control.stop();
                    state.control = Some(control);
                    return;
                }
                let lease = self.shared.ledger.reserve(
                    &control.installed().installation_id,
                    &[(Resource::HostBytes, 2 << 20)],
                );
                match lease {
                    Ok(lease) => {
                        state.binding = Some(Arc::new(Binding {
                            installed: control.installed().clone(),
                            manifest: control.shared_manifest(),
                            alive: AtomicBool::new(true),
                            _lease: lease,
                        }));
                        state.control = Some(control);
                        state.status = PluginStatus::Loading;
                        state.failure = None;
                    }
                    Err(error) => {
                        control.stop();
                        state.control = Some(control);
                        state.status = PluginStatus::Failed(error);
                        state.failure = Some(error);
                    }
                }
            }
            Err(error) => {
                if cancel.is_cancelled() {
                    state.restart = state.enabled;
                } else {
                    state.status = PluginStatus::Failed(error);
                    state.failure = Some(error);
                }
            }
        }
        self.dirty = true;
    }
    pub fn review(&mut self, window: Id, id: Id, path: PathBuf) -> Result<(), ErrorCode> {
        if self.review_slots.len() >= 2 {
            return Err(ErrorCode::Backpressure);
        }
        if !self.windows.contains_key(&window) {
            return Err(ErrorCode::Cancelled);
        }
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let installer = self.config.installer.clone();
        let resources = self.config.runtime.resources.clone();
        self.submit(true, "review", move || Work::Reviewed {
            id,
            result: guarded(|| Review::read(&path, &installer, &resources, &worker_cancel))
                .map(Box::new),
        })?;
        self.reviews.insert(
            id,
            PendingReview {
                window,
                cancel,
                expires: Deadline::after(Duration::from_secs(300)),
                review: None,
            },
        );
        self.review_slots.insert(id);
        Ok(())
    }
}
pub(super) fn remove_installation_data(setup: &Setup, installation_id: &str) {
    let scope = format!("plugin-v2.{installation_id}");
    for key in setup.vault.list_scope(&scope).into_keys() {
        let _ = setup.vault.set_secret(&scope, &key, None);
    }
    let _ = std::fs::remove_file(
        setup
            .state_directory
            .join(format!("{installation_id}.json")),
    );
    let _ = std::fs::remove_file(
        setup
            .state_directory
            .join(format!("{installation_id}.lock")),
    );
    let origins = setup.state_directory.join("origins");
    let _ = std::fs::remove_file(origins.join(format!("{installation_id}.json")));
    let _ = std::fs::remove_file(origins.join(format!("{installation_id}.lock")));
    let temp = setup.temporary_directory.join(installation_id);
    if temp.is_dir() {
        let _ = std::fs::remove_dir_all(temp);
    }
}
pub(super) fn guarded<T>(f: impl FnOnce() -> Result<T, ErrorCode>) -> Result<T, ErrorCode> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(Err(ErrorCode::Internal))
}
