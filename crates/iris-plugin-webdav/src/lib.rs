//! Official WebDAV provider. No filesystem, socket, thread, or process imports.
mod browse;
mod config;
mod media;
mod modal;
mod protocol;

use iris_plugin_sdk::{
    api::{
        SourceManageAction,
        v2::{
            ErrorCode, Generation, HostCommand, HostEvent, Id, task::Task, task_result::StoreRead,
        },
    },
    client::{Host, completed},
};
use protocol::WebDavConnection;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
struct WindowState {
    generation: Generation,
    jobs: BTreeMap<Id, browse::Job>,
    cache: Option<browse::Cache>,
    config: Option<config::Transaction>,
}
impl WindowState {
    fn cancel(&mut self, request: Id, host: &mut impl Host) {
        if let Some(job) = self.jobs.remove(&request) {
            job.cancel(host);
        }
    }
}
#[derive(Default)]
pub struct WebDav {
    servers: Vec<WebDavConnection>,
    loaded: bool,
    loading: Option<(Option<Id>, Id)>,
    config_owner: Option<Id>,
    windows: BTreeMap<Id, WindowState>,
}
impl WebDav {
    pub fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        match event {
            HostEvent::Init { .. } => {
                let _ = self.reload(None, host);
                Ok(Vec::new())
            }
            HostEvent::Completed {
                operation_id,
                result,
                ..
            } => self.loaded_result(None, operation_id, result),
            HostEvent::Window {
                window,
                generation,
                body,
            } => {
                if self.windows.len() >= 4 && !self.windows.contains_key(&window) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                let mut state = self.windows.remove(&window).unwrap_or_default();
                if state.generation != generation {
                    // The host revokes the old scope before delivering this one;
                    // directory pagination cache stays valid within the same context.
                    self.lost_scope(window);
                    let cache = (state.generation.context == generation.context)
                        .then(|| state.cache.take())
                        .flatten();
                    state = WindowState {
                        generation,
                        cache,
                        ..Default::default()
                    };
                }
                let result = self.window(window, &mut state, *body, host);
                self.windows.insert(window, state);
                result
            }
            HostEvent::WindowClosed { window } => {
                self.windows.remove(&window);
                self.lost_scope(window);
                Ok(Vec::new())
            }
            HostEvent::Shutdown => {
                *self = Self::default();
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }
    fn lost_scope(&mut self, window: Id) {
        if self.config_owner == Some(window) {
            // A cancelled write may already have committed. Reload before any
            // subsequent edit instead of guessing whether the disk was updated.
            self.config_owner = None;
            self.loaded = false;
        }
        if self.loading.is_some_and(|(scope, _)| scope == Some(window)) {
            self.loading = None;
        }
    }
    fn reload(&mut self, scope: Option<Id>, host: &mut impl Host) -> Result<(), ErrorCode> {
        if self.loading.is_none() {
            self.loading = Some((
                scope,
                host.submit(&Task::StateRead {
                    key: "servers".into(),
                })?,
            ));
        }
        Ok(())
    }
    fn loaded_result(
        &mut self,
        scope: Option<Id>,
        id: Id,
        value: Value,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        if self.loading != Some((scope, id)) {
            return Ok(Vec::new());
        }
        self.loading = None;
        let read: StoreRead = completed(value)?;
        self.servers = config::decode_servers(read.store_value.as_deref())?;
        self.loaded = true;
        Ok(vec![HostCommand::SetSources {
            sources: modal::source_descriptors(&self.servers),
        }])
    }
    fn window(
        &mut self,
        window: Id,
        state: &mut WindowState,
        event: HostEvent<Value>,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        if let HostEvent::Completed {
            operation_id,
            result,
            ..
        } = event
        {
            if self.loading == Some((Some(window), operation_id)) {
                return self.loaded_result(Some(window), operation_id, result);
            }
            if state
                .config
                .as_ref()
                .is_some_and(|c| c.operation == operation_id)
            {
                return self.config_completed(window, state, result, host);
            }
            let request = state
                .jobs
                .iter()
                .find_map(|(r, j)| (j.operation() == Some(operation_id)).then_some(*r));
            let Some(request) = request else {
                return Ok(Vec::new());
            };
            let mut job = state.jobs.remove(&request).unwrap();
            if !self.servers.iter().any(|c| c == &job.conn) {
                job.cancel(host);
                return Ok(browse::error(request, "连接配置已更改，请重新加载"));
            }
            return Ok(match job.complete(result, host) {
                Ok(browse::Progress::Pending(commands)) => {
                    state.jobs.insert(request, job);
                    commands
                }
                Ok(browse::Progress::Finished(commands, cache)) => {
                    if let Some(cache) = cache {
                        state.cache = Some(*cache);
                    }
                    commands
                }
                Err(code) => {
                    job.cancel(host);
                    browse::failure(request, code)
                }
            });
        }
        if let HostEvent::RequestCancelled { request_id } = event {
            state.cancel(request_id, host);
            return Ok(Vec::new());
        }
        if let HostEvent::OriginGranted {
            request_id,
            origin,
            grant,
            insecure_auth,
        } = event
        {
            let Some(mut job) = state.jobs.remove(&request_id) else {
                return Ok(Vec::new());
            };
            if !job.waiting_origin() {
                state.jobs.insert(request_id, job);
                return Ok(Vec::new());
            }
            if !self.servers.iter().any(|c| c == &job.conn) {
                job.cancel(host);
                return Ok(browse::error(request_id, "连接配置已更改，请重新加载"));
            }
            return Ok(match job.authorized(origin, grant, insecure_auth, host) {
                Ok(()) => {
                    state.jobs.insert(request_id, job);
                    Vec::new()
                }
                Err(code) => {
                    job.cancel(host);
                    browse::failure(request_id, code)
                }
            });
        }
        if !self.loaded {
            let _ = self.reload(Some(window), host);
            return Ok(match event {
                HostEvent::SourceBrowse { request_id, .. }
                | HostEvent::SourceResolve { request_id, .. } => {
                    browse::error(request_id, "正在恢复 WebDAV 配置，请稍后重试")
                }
                HostEvent::Command { .. }
                | HostEvent::SourceManage { .. }
                | HostEvent::ModalSubmit { .. } => modal::toast("正在恢复 WebDAV 配置，请稍后重试"),
                _ => Vec::new(),
            });
        }
        let commands = match event {
            HostEvent::Command { id, .. } | HostEvent::Action { id, .. } if id == "add_server" => {
                vec![HostCommand::OpenModal {
                    modal: Box::new(modal::build_modal(None, false)),
                }]
            }
            HostEvent::SourceManage { source_id, action } => {
                let Some(conn) = self.servers.iter().find(|c| c.id == source_id) else {
                    return Ok(modal::toast("未找到该 WebDAV 连接"));
                };
                match action {
                    SourceManageAction::Configure => vec![HostCommand::OpenModal {
                        modal: Box::new(modal::build_modal(
                            Some(conn),
                            conn.auth != protocol::WebDavAuth::None,
                        )),
                    }],
                    SourceManageAction::Remove => {
                        self.remove_config(window, state, &source_id, host)
                    }
                }
            }
            HostEvent::ModalSubmit { id, values } => {
                self.save_config(window, state, &id, &values, host)
            }
            HostEvent::SourceBrowse {
                request_id,
                source_id,
                folder_id,
                page,
                query,
                filters,
            } => {
                if !filters.is_empty() {
                    return Ok(browse::failure(request_id, ErrorCode::InvalidArgument));
                }
                let path = protocol::normalize_dav_path(folder_id.as_deref().unwrap_or("/"));
                if let Some(cursor) = page {
                    return Ok(state
                        .cache
                        .as_ref()
                        .ok_or(ErrorCode::StaleHandle)
                        .and_then(|cache| {
                            cache.page(request_id, &source_id, &path, query.as_deref(), &cursor)
                        })
                        .unwrap_or_else(|e| browse::failure(request_id, e)));
                }
                self.start_job(
                    state,
                    browse::Intent::Browse { path, query },
                    request_id,
                    &source_id,
                    host,
                )
            }
            HostEvent::SourceResolve {
                request_id,
                source_id,
                entry_id,
                variant,
                ..
            } => {
                if variant.is_some() {
                    return Ok(browse::failure(request_id, ErrorCode::Unsupported));
                }
                self.start_job(
                    state,
                    browse::Intent::Resolve {
                        entry: protocol::normalize_dav_path(&entry_id),
                    },
                    request_id,
                    &source_id,
                    host,
                )
            }
            _ => Vec::new(),
        };
        Ok(commands)
    }
    fn start_job(
        &self,
        state: &mut WindowState,
        intent: browse::Intent,
        request: Id,
        source: &str,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        let count = state.jobs.len() + self.windows.values().map(|w| w.jobs.len()).sum::<usize>();
        let same_intent = state.jobs.values().any(|j| j.same_intent(&intent));
        if same_intent || count >= 8 || state.jobs.contains_key(&request) {
            return browse::failure(request, ErrorCode::Backpressure);
        }
        let Some(conn) = self.servers.iter().find(|c| c.id == source) else {
            return browse::error(request, "连接已被移除");
        };
        match browse::Job::start(request, conn.clone(), intent, state.cache.as_ref(), host) {
            Ok(job) => {
                state.jobs.insert(request, job);
                Vec::new()
            }
            Err(e) => browse::failure(request, e),
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use iris_plugin_sdk::api::v2::{GuestError, Reply};
    std::thread_local! { static PLUGIN: std::cell::RefCell<WebDav> = std::cell::RefCell::new(WebDav::default()); }
    fn dispatch(value: Value) -> Reply<HostCommand> {
        let result = serde_json::from_value(value)
            .map_err(|_| ErrorCode::InvalidArgument)
            .and_then(|event| {
                PLUGIN.with_borrow_mut(|plugin| {
                    plugin.event(event, &mut iris_plugin_sdk::client::WasmHost)
                })
            });
        match result {
            Ok(ok) => Reply::Ok { ok },
            Err(code) => Reply::Error {
                error: GuestError {
                    code: code as i32,
                    message: format!("{code:?}"),
                },
            },
        }
    }
    iris_plugin_sdk::export_plugin!(dispatch);
}

#[cfg(test)]
mod tests;
