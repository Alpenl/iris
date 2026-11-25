//! Official composition guides, running through the public Wasm SDK.
mod view;
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, Generation, HostCommand, HostEvent, Id,
        task::Task,
        task_result::{StoreRead, StoreWrite},
    },
    client::{Host, completed},
    serde_json::Value,
};
use std::collections::BTreeMap;
use view::{GuideMode, GuidesView};

enum Pending {
    Read { id: Id, tries: u8 },
    Write { id: Id, value: String, tries: u8 },
}
impl Pending {
    fn id(&self) -> Id {
        match self {
            Self::Read { id, .. } | Self::Write { id, .. } => *id,
        }
    }
}
#[derive(Default)]
struct WindowState {
    generation: Generation,
    view: GuidesView,
    read_done: bool,
    changed: bool,
    pending: Option<Pending>,
    desired: Option<String>,
}
#[derive(Default)]
pub struct Guides {
    windows: BTreeMap<Id, WindowState>,
}
impl Guides {
    pub fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        match event {
            HostEvent::Window {
                window,
                generation,
                body,
            } => {
                if self.windows.len() >= 4 && !self.windows.contains_key(&window) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                let state = self.windows.entry(window).or_default();
                if state.generation != generation {
                    // The host has already revoked old scoped operations. Do not
                    // call close in the new scope using an old scope's handles.
                    if let Some(Pending::Write { value, .. }) = state.pending.take()
                        && state.desired.is_none()
                    {
                        state.desired = Some(value);
                    }
                    state.generation = generation;
                }
                state.event(*body, host)
            }
            HostEvent::WindowClosed { window } => {
                self.windows.remove(&window);
                Ok(Vec::new())
            }
            HostEvent::Shutdown => {
                self.windows.clear();
                Ok(Vec::new())
            }
            HostEvent::Init { .. } => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }
}
impl WindowState {
    fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let mut commands = Vec::new();
        match event {
            HostEvent::WindowOpened { .. } => {
                if let Ok(id) = host.submit(&Task::StateRead { key: "mode".into() }) {
                    self.pending = Some(Pending::Read { id, tries: 0 });
                }
                commands = self.view.sync_ui(false);
            }
            HostEvent::Command { id, .. } | HostEvent::Action { id, .. } => {
                let mode = if id == "cycle" {
                    self.view.mode.next()
                } else {
                    let Some(mode) = GuideMode::from_key(&id) else {
                        return Ok(commands);
                    };
                    mode
                };
                self.view.mode = mode;
                self.changed = true;
                self.read_done = true;
                self.desired = Some(mode.as_key().into());
                commands = self.view.sync_ui(true);
            }
            HostEvent::Completed {
                operation_id,
                result,
                ..
            } => {
                if self.pending.as_ref().is_none_or(|p| p.id() != operation_id) {
                    if !self.read_done
                        && !self.changed
                        && self.pending.is_none()
                        && let Ok(id) = host.submit(&Task::StateRead { key: "mode".into() })
                    {
                        self.pending = Some(Pending::Read { id, tries: 0 });
                    }
                    return Ok(commands);
                }
                match self.pending.take().unwrap() {
                    Pending::Read { tries, .. } => match completed::<StoreRead>(result) {
                        Ok(read) if !self.changed => {
                            self.read_done = true;
                            self.view.mode = read
                                .store_value
                                .as_deref()
                                .and_then(GuideMode::from_key)
                                .unwrap_or_default();
                            commands = self.view.sync_ui(false);
                        }
                        Ok(_) => self.read_done = true,
                        Err(ErrorCode::Backpressure) if tries < 3 => {
                            if let Ok(id) = host.submit(&Task::StateRead { key: "mode".into() }) {
                                self.pending = Some(Pending::Read {
                                    id,
                                    tries: tries + 1,
                                });
                            }
                        }
                        Err(_) => {
                            self.read_done = true;
                            commands.push(toast("辅助线配置未读取，当前使用关闭状态"));
                        }
                    },
                    Pending::Write { value, tries, .. } => match completed::<StoreWrite>(result) {
                        Ok(_) => {}
                        Err(ErrorCode::Backpressure) if tries < 3 && self.desired.is_none() => {
                            if let Ok(id) = host.submit(&Task::StateWrite {
                                key: "mode".into(),
                                value: Some(value.clone()),
                            }) {
                                self.pending = Some(Pending::Write {
                                    id,
                                    value,
                                    tries: tries + 1,
                                });
                            } else {
                                self.desired = Some(value);
                            }
                        }
                        Err(_) if self.desired.is_none() => {
                            commands.push(toast("辅助线模式已切换，但配置尚未保存"))
                        }
                        Err(_) => {}
                    },
                }
            }
            _ => {
                if !self.read_done
                    && !self.changed
                    && self.pending.is_none()
                    && let Ok(id) = host.submit(&Task::StateRead { key: "mode".into() })
                {
                    self.pending = Some(Pending::Read { id, tries: 0 });
                }
            }
        }
        if self.pending.is_none()
            && let Some(value) = self.desired.take()
        {
            match host.submit(&Task::StateWrite {
                key: "mode".into(),
                value: Some(value.clone()),
            }) {
                Ok(id) => {
                    self.pending = Some(Pending::Write {
                        id,
                        value,
                        tries: 0,
                    })
                }
                Err(_) => commands.push(toast("辅助线模式已切换，但配置尚未保存")),
            }
        }
        Ok(commands)
    }
}
fn toast(message: &str) -> HostCommand {
    HostCommand::ShowToast {
        message: message.into(),
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use iris_plugin_sdk::api::v2::{GuestError, Reply};
    std::thread_local! { static PLUGIN: std::cell::RefCell<Guides> = std::cell::RefCell::new(Guides::default()); }
    fn dispatch(value: Value) -> Reply<HostCommand> {
        let result = iris_plugin_sdk::serde_json::from_value(value)
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
