//! Per-window session timer: no network or filesystem capability. Persisted
//! asynchronous state is demonstrated by the official Guides/WebDAV sources.
use iris_plugin_sdk::{
    api::{
        StatusBadge,
        v2::{Generation, HostCommand, HostEvent, Id},
    },
    serde_json::Value,
};
use std::collections::BTreeMap;
#[derive(Default)]
pub struct Stats {
    windows: BTreeMap<Id, Timer>,
}
#[derive(Default)]
struct Timer {
    generation: Generation,
    last: Option<u64>,
    watched: u64,
}
impl Stats {
    pub fn event(&mut self, event: HostEvent<Value>, now: u64) -> Vec<HostCommand> {
        match event {
            HostEvent::WindowClosed { window } => {
                self.windows.remove(&window);
                Vec::new()
            }
            HostEvent::Window {
                window,
                generation,
                body,
            } => {
                if !self.windows.contains_key(&window) && self.windows.len() >= 4 {
                    return Vec::new();
                }
                let timer = self.windows.entry(window).or_default();
                if timer.generation != generation {
                    timer.generation = generation;
                    timer.last = None;
                }
                match *body {
                    HostEvent::Tick { paused, .. } => {
                        if !paused && let Some(last) = timer.last {
                            timer.watched = timer
                                .watched
                                .saturating_add(now.saturating_sub(last).min(5000));
                        }
                        timer.last = (!paused).then_some(now);
                    }
                    HostEvent::WindowOpened { .. } | HostEvent::MediaOpened { .. } => {
                        timer.last = None
                    }
                    HostEvent::Command { id, .. } | HostEvent::Action { id, .. }
                        if id == "reset" =>
                    {
                        timer.watched = 0;
                        timer.last = None;
                    }
                    _ => return Vec::new(),
                }
                vec![HostCommand::SetStatus {
                    badge: Some(Box::new(StatusBadge {
                        id: "watch-timer".into(),
                        text: format!("本窗口已看 {} 秒", timer.watched / 1000),
                        tooltip: Some("点击重置本次窗口计时".into()),
                        accent: false,
                        action_id: Some("reset".into()),
                    })),
                }]
            }
            _ => Vec::new(),
        }
    }
}
#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use iris_plugin_sdk::{
        api::v2::{ErrorCode, GuestError, Reply},
        guest,
    };
    std::thread_local! {static APP:std::cell::RefCell<Stats>=std::cell::RefCell::new(Stats::default());}
    fn dispatch(value: Value) -> Reply<HostCommand> {
        let result = iris_plugin_sdk::serde_json::from_value(value)
            .map_err(|_| ErrorCode::InvalidArgument)
            .and_then(|event| {
                let now = guest::clock(true)?.0;
                Ok(APP.with_borrow_mut(|app| app.event(event, now)))
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
