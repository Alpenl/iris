//! Single-owner modal submission/validation state and declarative `ViewTimer`
//! domain resolution for [`super::PluginController`].

use std::collections::BTreeMap;
use std::future::Future;

use iris_api::{PluginModal, SidebarPanel};

/// Consolidated async submission, inline validation error, and focus-dirty state
/// for plugin-owned modals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginModalState {
    submitting: Option<(String, String)>,
    error: Option<String>,
    focus_pending: bool,
}

impl PluginModalState {
    pub fn begin_submit(
        &mut self,
        pid: &str,
        modal: &PluginModal,
        values: BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        if self
            .submitting
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == &modal.id)
        {
            return None;
        }
        self.error = None;
        self.submitting = Some((pid.to_owned(), modal.id.clone()));
        Some(values)
    }

    pub fn clear_submitting_for(&mut self, pid: &str) -> bool {
        if self.submitting.as_ref().is_some_and(|(p, _)| p == pid) {
            self.submitting = None;
            return true;
        }
        false
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn clear_feedback(&mut self) {
        self.submitting = None;
        self.error = None;
    }

    pub fn on_cancel(&mut self) {
        self.submitting = None;
        self.error = None;
        self.focus_pending = true;
    }

    pub fn on_dirty(&mut self) {
        self.submitting = None;
        self.error = None;
        self.focus_pending = true;
    }

    pub fn mark_focus_pending(&mut self) {
        self.focus_pending = true;
    }

    pub fn focus_pending(&self) -> bool {
        self.focus_pending
    }

    pub fn take_focus_pending(&mut self) -> bool {
        std::mem::take(&mut self.focus_pending)
    }

    pub fn is_submitting(&self, pid: &str, modal_id: &str) -> bool {
        self.submitting
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == modal_id)
    }
}

/// Identifies a declarative `ViewTimer` slot owned by an active modal or sidebar panel.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ViewTimerSlot {
    Modal(String),
    Sidebar(String),
}

pub(super) fn clamp_timer_interval_ms(interval_ms: u32) -> u32 {
    interval_ms.clamp(1_000, 300_000)
}

pub(super) fn resolve_modal_timer(
    has_host_consent: bool,
    active_modal: Option<(&str, &PluginModal)>,
) -> Option<(String, String, u32)> {
    if has_host_consent {
        return None;
    }
    let (pid, modal) = active_modal?;
    let timer = modal.timer.as_ref()?;
    if timer.action_id.is_empty() {
        return None;
    }
    Some((
        pid.to_owned(),
        timer.action_id.clone(),
        clamp_timer_interval_ms(timer.interval_ms),
    ))
}

pub(super) fn resolve_sidebar_timers(
    has_host_consent: bool,
    active_modal: Option<(&str, &PluginModal)>,
    sidebar_panels: &[(&str, &SidebarPanel)],
) -> Vec<(String, String, u32)> {
    if has_host_consent {
        return Vec::new();
    }
    let modal_action = active_modal.and_then(|(pid, modal)| {
        modal
            .timer
            .as_ref()
            .filter(|t| !t.action_id.is_empty())
            .map(|t| (pid, t.action_id.as_str()))
    });
    let mut out = Vec::new();
    for &(pid, panel) in sidebar_panels {
        if let Some(timer) = &panel.timer
            && !timer.action_id.is_empty()
            && modal_action != Some((pid, timer.action_id.as_str()))
        {
            out.push((
                pid.to_owned(),
                timer.action_id.clone(),
                clamp_timer_interval_ms(timer.interval_ms),
            ));
        }
    }
    out
}

/// Runs a periodic `ViewTimer` loop until `tick()` returns `false` or the future is dropped.
pub async fn run_view_timer<F: Future<Output = ()>>(
    mut wait: impl FnMut() -> F,
    mut tick: impl FnMut() -> bool,
) {
    loop {
        wait().await;
        if !tick() {
            break;
        }
    }
}
