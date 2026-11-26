//! Revoke account work before deleting or switching credentials across windows.
use crate::{
    BaiduPanPlugin, SECRET_KEY, WindowState,
    auth::{QuotaInfo, StoredSession},
    ui,
};
use iris_plugin_sdk::{
    api::v2::{
        HostCommand, HostEvent, Id,
        task::{CredentialBucket, Task},
        task_result::{HttpResult, ReadResult},
    },
    client::{Host, completed},
    serde_json::{self, Value},
};
use std::collections::BTreeSet;

pub(crate) enum InterruptedSecret {
    Login {
        epoch: u64,
        session: Box<StoredSession>,
        quota: Option<QuotaInfo>,
        close_modal: bool,
        notify_login: bool,
    },
    Background {
        epoch: u64,
    },
    Logout {
        epoch: u64,
    },
}

impl WindowState {
    /// Drop local scoped tracking after the host has already revoked old-generation handles,
    /// recording any unacknowledged credential write so it is re-submitted in the new scope.
    pub(super) fn discard_scoped(&mut self) {
        self.http_jobs.clear();
        self.pending_subtitles.clear();
        if let Some((_, epoch, session, quota, close_modal, notify_login)) =
            self.secret_write_op.take()
        {
            self.interrupted_secret = Some(InterruptedSecret::Login {
                epoch,
                session: Box::new(session),
                quota,
                close_modal,
                notify_login,
            });
            self.logout_secret_op = None;
            self.background_secret_ops.clear();
            self.background_secret_epoch = None;
        } else if let Some((_, epoch)) = self.logout_secret_op.take() {
            self.interrupted_secret = Some(InterruptedSecret::Logout { epoch });
            self.background_secret_ops.clear();
            self.background_secret_epoch = None;
        } else if !self.background_secret_ops.is_empty() {
            self.background_secret_ops.clear();
            if let Some(epoch) = self.background_secret_epoch.take() {
                self.interrupted_secret = Some(InterruptedSecret::Background { epoch });
            }
        }
    }

    pub(super) fn clear_scoped(&mut self, host: &mut impl Host) {
        for (_, job) in std::mem::take(&mut self.http_jobs) {
            job.cancel(host);
        }
        self.pending_subtitles.clear();
        if let Some((op, ..)) = self.secret_write_op.take() {
            host.close(op);
        }
        if let Some((op, _)) = self.logout_secret_op.take() {
            host.close(op);
        }
        for op in std::mem::take(&mut self.background_secret_ops) {
            host.close(op);
        }
        self.background_secret_epoch = None;
        self.interrupted_secret = None;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_secret_write(
        &mut self,
        op: Id,
        epoch: u64,
        session: StoredSession,
        quota: Option<QuotaInfo>,
        close_modal: bool,
        notify_login: bool,
        host: &mut impl Host,
    ) {
        self.interrupted_secret = None;
        if let Some((logout_op, _)) = self.logout_secret_op.take()
            && logout_op != op
        {
            host.close(logout_op);
        }
        for bg_op in std::mem::take(&mut self.background_secret_ops) {
            if bg_op != op {
                host.close(bg_op);
            }
        }
        self.background_secret_epoch = None;
        if let Some((prev_op, ..)) =
            self.secret_write_op
                .replace((op, epoch, session, quota, close_modal, notify_login))
            && prev_op != op
        {
            host.close(prev_op);
        }
    }

    pub(crate) fn submit_logout_secret(&mut self, epoch: u64, host: &mut impl Host) {
        self.interrupted_secret = None;
        if let Some((prev_op, ..)) = self.secret_write_op.take() {
            host.close(prev_op);
        }
        for bg_op in std::mem::take(&mut self.background_secret_ops) {
            host.close(bg_op);
        }
        self.background_secret_epoch = None;
        match host.submit(&Task::SecretWrite {
            bucket: CredentialBucket::Plugin,
            key: SECRET_KEY.into(),
            value: None,
        }) {
            Ok(op) => {
                if let Some((prev, _)) = self.logout_secret_op.replace((op, epoch))
                    && prev != op
                {
                    host.close(prev);
                }
            }
            Err(_) => {
                if let Some((prev, _)) = self.logout_secret_op.take() {
                    host.close(prev);
                }
                self.interrupted_secret = Some(InterruptedSecret::Logout { epoch });
            }
        }
    }

    pub(crate) fn submit_background_secret(
        &mut self,
        session: &StoredSession,
        epoch: u64,
        host: &mut impl Host,
    ) {
        if self.secret_write_op.is_some() || self.logout_secret_op.is_some() {
            return;
        }
        self.interrupted_secret = None;
        let Ok(raw) = serde_json::to_string(session) else {
            return;
        };
        match host.submit(&Task::SecretWrite {
            bucket: CredentialBucket::Plugin,
            key: SECRET_KEY.into(),
            value: Some(raw),
        }) {
            Ok(op) => {
                self.background_secret_ops.insert(op);
                self.background_secret_epoch = Some(epoch);
            }
            Err(_) => {
                self.interrupted_secret = Some(InterruptedSecret::Background { epoch });
            }
        }
    }
}

impl BaiduPanPlugin {
    pub(super) fn resume_interrupted_secret(&self, state: &mut WindowState, host: &mut impl Host) {
        let Some(pending) = state.interrupted_secret.take() else {
            return;
        };
        match pending {
            InterruptedSecret::Login {
                epoch,
                session,
                quota,
                close_modal,
                notify_login,
            } if epoch == self.account_epoch => {
                let Ok(encoded) = serde_json::to_string(&*session) else {
                    return;
                };
                match host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: SECRET_KEY.into(),
                    value: Some(encoded),
                }) {
                    Ok(op) => {
                        state.secret_write_op =
                            Some((op, epoch, *session, quota, close_modal, notify_login));
                    }
                    Err(_) => {
                        state.interrupted_secret = Some(InterruptedSecret::Login {
                            epoch,
                            session,
                            quota,
                            close_modal,
                            notify_login,
                        });
                    }
                }
            }
            InterruptedSecret::Background { epoch }
                if epoch == self.account_epoch && self.session.is_authenticated() =>
            {
                state.submit_background_secret(&self.session, epoch, host);
            }
            InterruptedSecret::Logout { epoch }
                if epoch == self.account_epoch && !self.session.is_authenticated() =>
            {
                state.submit_logout_secret(epoch, host);
            }
            _ => {}
        }
    }

    pub(super) fn reset_account(
        &mut self,
        state: &mut WindowState,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        // An installation-scoped read cannot be closed from a window. Forget its
        // ID so an already queued initialization reply cannot restore the account.
        self.init_secret_op = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
        state.account_reset = true;
        for other in self.windows.values_mut() {
            other.account_reset = true;
        }
        self.session = StoredSession::default();
        self.quota = None;
        self.video_lookup.clear();
        state.last_resolved_video = None;
        self.flush_account_reset(state, None, host)
    }

    pub(crate) fn commit_session(
        &mut self,
        state: &mut WindowState,
        next_session: StoredSession,
        next_quota: Option<QuotaInfo>,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        self.init_secret_op = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
        for other in self.windows.values_mut() {
            other.account_reset = true;
        }
        self.session = next_session;
        self.quota = next_quota;
        self.video_lookup.clear();

        let requests: BTreeSet<_> = state
            .http_jobs
            .values()
            .filter_map(|job| job.request_id)
            .collect();
        state.account_reset = false;
        state.clear_scoped(host);
        state.qr = None;
        state.folder_caches.clear();
        state.browse_seen.clear();
        state.browse_cursors.clear();
        state.browse_page = None;
        state.last_resolved_video = None;

        requests
            .into_iter()
            .map(|request_id| HostCommand::SourceError {
                request_id,
                message: "账号已切换，请刷新目录".into(),
            })
            .collect()
    }

    pub(super) fn flush_account_reset(
        &self,
        state: &mut WindowState,
        event: Option<&HostEvent<Value>>,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        if !std::mem::take(&mut state.account_reset) {
            return Vec::new();
        }
        // Only the owning window may close its operations. If its next event is
        // a completion, the host has already transferred the new handle/blob.
        if let Some(HostEvent::Completed {
            operation_id,
            result,
            ..
        }) = event
            && let Some(job) = state.http_jobs.get(operation_id)
        {
            if job.stream.is_none() {
                if let Ok(HttpResult::Response { stream_handle, .. }) = completed(result.clone()) {
                    host.close(stream_handle);
                }
            } else if let Ok(read) = completed::<ReadResult>(result.clone()) {
                host.close(read.blob);
            }
        }
        let cancelled = match event {
            Some(HostEvent::RequestCancelled { request_id }) => Some(*request_id),
            _ => None,
        };
        let requests: BTreeSet<_> = state
            .http_jobs
            .values()
            .filter_map(|job| job.request_id)
            .filter(|id| Some(*id) != cancelled)
            .collect();
        state.clear_scoped(host);
        state.qr = None;
        state.folder_caches.clear();
        state.browse_seen.clear();
        state.browse_cursors.clear();
        state.browse_page = None;
        state.last_resolved_video = None;
        let msg = if self.session.is_authenticated() {
            "账号已切换，请刷新目录"
        } else {
            "账号已退出，请重新登录或刷新目录"
        };
        let mut commands: Vec<_> = requests
            .into_iter()
            .map(|request_id| HostCommand::SourceError {
                request_id,
                message: msg.into(),
            })
            .collect();
        commands.extend(self.sync_ui(state));
        commands.extend(ui::refresh_all_sources());
        commands
    }
}
