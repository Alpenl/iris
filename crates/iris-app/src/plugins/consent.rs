//! Host-owned package installation review and dynamic Origin authorization state
//! for [`super::PluginController`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_api::v2::{ErrorCode, Generation, Id};
use iris_plugin_host::{OriginSummary, ReviewSummary, WindowSession};

use super::modal::PluginModalState;
use super::ports::{PluginBatchOutcome, error_code_label};

/// Maximum duration an unapproved package installation review remains open before auto-dismissal.
pub const REVIEW_EXPIRY: Duration = Duration::from_secs(300);

#[derive(Default)]
pub(super) struct HostConsentState {
    pub review: Option<Arc<ReviewSummary>>,
    pub review_created: Option<Instant>,
    pub origin: Option<Arc<OriginSummary>>,
    pub insecure_auth: bool,
    pub keep_identity: bool,
    pub consent_busy: BTreeSet<Id>,
}

impl HostConsentState {
    pub fn has_active(&self) -> bool {
        self.review.is_some() || self.origin.is_some()
    }

    pub fn active_id(&self) -> Option<Id> {
        self.origin
            .as_ref()
            .map(|o| o.id)
            .or_else(|| self.review.as_ref().map(|r| r.id))
    }

    pub fn is_busy(&self, id: Id) -> bool {
        self.consent_busy.contains(&id)
    }

    pub fn toggle_insecure_auth(&mut self, origin_id: Id) -> bool {
        if self.consent_busy.contains(&origin_id) {
            return false;
        }
        self.insecure_auth = !self.insecure_auth;
        true
    }

    pub fn toggle_keep_identity(&mut self, review_id: Id) -> bool {
        if self.consent_busy.contains(&review_id) {
            return false;
        }
        self.keep_identity = !self.keep_identity;
        true
    }

    pub fn review_package(window: &WindowSession, path: PathBuf) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("irisplugin"))
        {
            out.push_toast("请选择 .irisplugin 插件包", false);
            return out;
        }
        match window.review_package(path) {
            Ok(_) => out.push_toast("正在验证插件包…", false),
            Err(error) => {
                log::warn!("plugin review rejected: {error:?}");
                out.push_toast(
                    format!("无法审核插件包 · {}", error_code_label(error)),
                    true,
                );
            }
        }
        out
    }

    pub fn accept_review(
        &mut self,
        window: &WindowSession,
        review: Arc<ReviewSummary>,
        now: Instant,
    ) -> PluginBatchOutcome {
        let new_id = review.id;
        if let Some(old) = self.review.replace(review) {
            let _ = window.dismiss_review(old.id);
            self.consent_busy.remove(&old.id);
        }
        self.consent_busy.remove(&new_id);
        self.review_created = Some(now);
        self.keep_identity = false;
        let mut out = PluginBatchOutcome::default();
        out.domain.consent_changed = true;
        out.domain.close_overlays = true;
        out
    }

    pub fn accept_origin(
        &mut self,
        window: &WindowSession,
        origin: Arc<OriginSummary>,
    ) -> PluginBatchOutcome {
        let new_id = origin.id;
        if let Some(old) = self.origin.replace(origin) {
            let _ = window.confirm_origin(old.id, false, false);
            self.consent_busy.remove(&old.id);
        }
        self.consent_busy.remove(&new_id);
        self.insecure_auth = false;
        let mut out = PluginBatchOutcome::default();
        out.domain.consent_changed = true;
        out.domain.close_overlays = true;
        out
    }

    pub fn close_origin(
        &mut self,
        id: Id,
        has_active_modal: bool,
        modal: &mut PluginModalState,
    ) -> PluginBatchOutcome {
        self.consent_busy.remove(&id);
        let mut out = PluginBatchOutcome::default();
        if self.origin.as_ref().is_some_and(|o| o.id == id) {
            self.origin = None;
            out.domain.consent_changed = true;
            if !self.has_active() && has_active_modal {
                modal.mark_focus_pending();
                out.domain.modal_changed = true;
            }
        }
        out
    }

    pub fn on_installed(
        &mut self,
        plugin_id: &str,
        has_active_modal: bool,
        modal: &mut PluginModalState,
    ) -> PluginBatchOutcome {
        if let Some(old) = self.review.take() {
            self.consent_busy.remove(&old.id);
        }
        self.review_created = None;
        let mut out = PluginBatchOutcome::default();
        out.domain.consent_changed = true;
        if !self.has_active() && has_active_modal {
            modal.mark_focus_pending();
            out.domain.modal_changed = true;
        }
        out.push_toast(format!("已安装 {plugin_id}"), false);
        out
    }

    pub fn on_request_error(
        &mut self,
        request: Option<Id>,
        code: ErrorCode,
        has_active_modal: bool,
        modal: &mut PluginModalState,
    ) -> bool {
        let mut changed = false;
        if let Some(id) = request {
            if self.consent_busy.remove(&id) {
                changed = true;
            }
            if code != ErrorCode::Backpressure && self.review.as_ref().is_some_and(|r| r.id == id) {
                self.review = None;
                self.review_created = None;
                changed = true;
                if !self.has_active() && has_active_modal {
                    modal.mark_focus_pending();
                }
            }
        }
        changed
    }

    pub fn clear_origin_on_media_advance(
        &mut self,
        connected: bool,
        has_active_modal: bool,
        modal: &mut PluginModalState,
    ) -> bool {
        if connected && let Some(origin) = self.origin.take() {
            self.consent_busy.remove(&origin.id);
            if !self.has_active() && has_active_modal {
                modal.mark_focus_pending();
            }
            return true;
        }
        false
    }

    pub fn cancel_front(
        &mut self,
        window: &WindowSession,
        has_active_modal: bool,
        modal: &mut PluginModalState,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if let Some(origin) = self.origin.take() {
            let _ = window.confirm_origin(origin.id, false, false);
            self.consent_busy.remove(&origin.id);
            out.domain.consent_changed = true;
        } else if let Some(review) = self.review.take() {
            let _ = window.dismiss_review(review.id);
            self.review_created = None;
            self.consent_busy.remove(&review.id);
            out.domain.consent_changed = true;
        }
        if !self.has_active() && has_active_modal {
            modal.mark_focus_pending();
            out.domain.modal_changed = true;
        }
        out
    }

    pub fn approve_front(&mut self, window: &WindowSession) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        let (target_id, result) = if let Some(origin) = &self.origin {
            if self.consent_busy.contains(&origin.id) {
                return out;
            }
            (
                origin.id,
                window.confirm_origin(origin.id, true, self.insecure_auth),
            )
        } else if let Some(review) = &self.review {
            if self.consent_busy.contains(&review.id) {
                return out;
            }
            (
                review.id,
                window.approve_review(
                    review.id,
                    review.manifest.permissions.iter().copied().collect(),
                    self.keep_identity,
                ),
            )
        } else {
            return out;
        };
        match result {
            Ok(()) => {
                self.consent_busy.insert(target_id);
                out.domain.consent_changed = true;
            }
            Err(error) => {
                log::warn!("host consent submit failed: {error:?}");
                out.domain.consent_changed = true;
                out.push_toast(format!("授权未提交 · {}", error_code_label(error)), true);
            }
        }
        out
    }

    pub fn reconcile(
        &mut self,
        window: &WindowSession,
        current_gen: Generation,
        has_active_modal: bool,
        modal: &mut PluginModalState,
        now: Instant,
    ) -> PluginBatchOutcome {
        let mut out = PluginBatchOutcome::default();
        if let Some(review) = self
            .review
            .as_ref()
            .filter(|_| {
                self.review_created
                    .is_some_and(|t| now.saturating_duration_since(t) >= REVIEW_EXPIRY)
            })
            .cloned()
        {
            let _ = window.dismiss_review(review.id);
            self.review = None;
            self.review_created = None;
            self.consent_busy.remove(&review.id);
            out.domain.consent_changed = true;
            if !self.has_active() && has_active_modal {
                modal.mark_focus_pending();
                out.domain.modal_changed = true;
            }
        }
        if self
            .origin
            .as_ref()
            .is_some_and(|r| !r.binding.is_live() || r.generation != current_gen)
        {
            if let Some(origin) = self.origin.take() {
                self.consent_busy.remove(&origin.id);
            }
            out.domain.consent_changed = true;
            if !self.has_active() && has_active_modal {
                modal.mark_focus_pending();
                out.domain.modal_changed = true;
            }
        }
        out
    }
}
