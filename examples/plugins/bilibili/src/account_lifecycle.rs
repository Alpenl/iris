//! Revoke account work before deleting credentials, including work in other windows.
use crate::{BilibiliPlugin, WindowState, auth::StoredSession, ui};
use iris_plugin_sdk::{
    api::v2::{
        HostCommand, HostEvent, Id,
        task::{CredentialBucket, Task},
        task_result::{ArtifactResult, ArtifactWriterResult, HttpResult, ReadResult},
    },
    client::{Host, completed},
    serde_json::{self, Value},
};
use std::collections::BTreeSet;

pub(crate) enum InterruptedSecret {
    Login {
        account_epoch: u64,
        session: StoredSession,
        close_modal: bool,
    },
    Background {
        account_epoch: u64,
    },
    Logout {
        account_epoch: u64,
    },
}

impl WindowState {
    /// Drop local scoped tracking after the host has already revoked old-generation handles,
    /// recording any unacknowledged credential write so it is re-submitted in the new scope.
    pub(super) fn discard_scoped(&mut self) {
        self.http_jobs.clear();
        self.sub_jobs.clear();
        self.pending_subtitles.clear();
        self.pending_wbi_browse.clear();
        self.pending_wbi_playurl.clear();
        if let Some((_, account_epoch, session, close_modal)) = self.secret_write_op.take() {
            self.interrupted_secret = Some(InterruptedSecret::Login {
                account_epoch,
                session,
                close_modal,
            });
            self.logout_secret_op = None;
            self.background_secret_ops.clear();
            self.background_secret_epoch = None;
        } else if let Some((_, account_epoch)) = self.logout_secret_op.take() {
            self.interrupted_secret = Some(InterruptedSecret::Logout { account_epoch });
            self.background_secret_ops.clear();
            self.background_secret_epoch = None;
        } else if !self.background_secret_ops.is_empty() {
            self.background_secret_ops.clear();
            if let Some(account_epoch) = self.background_secret_epoch.take() {
                self.interrupted_secret = Some(InterruptedSecret::Background { account_epoch });
            }
        }
        self.needs_wbi_player_v2 = false;
    }

    pub(super) fn clear_scoped(&mut self, host: &mut impl Host) {
        for (_, job) in std::mem::take(&mut self.http_jobs) {
            job.cancel(host);
        }
        for (_, sub) in std::mem::take(&mut self.sub_jobs) {
            host.close(sub.op);
            if let Some(writer) = sub.writer {
                host.close(writer);
            }
        }
        self.pending_subtitles.clear();
        self.pending_wbi_browse.clear();
        self.pending_wbi_playurl.clear();
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
        self.needs_wbi_player_v2 = false;
    }

    pub(super) fn set_secret_write(
        &mut self,
        op: Id,
        account_epoch: u64,
        session: StoredSession,
        close_modal: bool,
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
                .replace((op, account_epoch, session, close_modal))
            && prev_op != op
        {
            host.close(prev_op);
        }
    }

    pub(crate) fn submit_logout_secret(&mut self, account_epoch: u64, host: &mut impl Host) {
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
            key: "bili.session".into(),
            value: None,
        }) {
            Ok(op) => {
                if let Some((prev, _)) = self.logout_secret_op.replace((op, account_epoch))
                    && prev != op
                {
                    host.close(prev);
                }
            }
            Err(_) => {
                if let Some((prev, _)) = self.logout_secret_op.take() {
                    host.close(prev);
                }
                self.interrupted_secret = Some(InterruptedSecret::Logout { account_epoch });
            }
        }
    }

    pub(crate) fn submit_background_secret(
        &mut self,
        session: &StoredSession,
        account_epoch: u64,
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
            key: "bili.session".into(),
            value: Some(raw),
        }) {
            Ok(op) => {
                self.background_secret_ops.insert(op);
                self.background_secret_epoch = Some(account_epoch);
            }
            Err(_) => {
                self.interrupted_secret = Some(InterruptedSecret::Background { account_epoch });
            }
        }
    }
}

impl BilibiliPlugin {
    pub(super) fn resume_interrupted_secret(&self, state: &mut WindowState, host: &mut impl Host) {
        let Some(pending) = state.interrupted_secret.take() else {
            return;
        };
        match pending {
            InterruptedSecret::Login {
                account_epoch,
                session,
                close_modal,
            } if account_epoch == self.account_epoch => {
                let Ok(encoded) = serde_json::to_string(&session) else {
                    return;
                };
                match host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: "bili.session".into(),
                    value: Some(encoded),
                }) {
                    Ok(op) => {
                        state.secret_write_op = Some((op, account_epoch, session, close_modal));
                    }
                    Err(_) => {
                        state.interrupted_secret = Some(InterruptedSecret::Login {
                            account_epoch,
                            session,
                            close_modal,
                        });
                    }
                }
            }
            InterruptedSecret::Background { account_epoch }
                if account_epoch == self.account_epoch && self.session.is_authenticated() =>
            {
                state.submit_background_secret(&self.session, account_epoch, host);
            }
            InterruptedSecret::Logout { account_epoch }
                if account_epoch == self.account_epoch && !self.session.is_authenticated() =>
            {
                state.submit_logout_secret(account_epoch, host);
            }
            _ => {}
        }
    }

    pub(super) fn reset_account(
        &mut self,
        state: &mut WindowState,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        // Ignore a late installation-scoped read; closing it from a window is
        // forbidden by the host's handle ownership checks.
        self.init_secret_op = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
        state.account_reset = true;
        for other in self.windows.values_mut() {
            other.account_reset = true;
        }
        self.session = StoredSession {
            buvid3: std::mem::take(&mut self.session.buvid3),
            ..Default::default()
        };
        self.cid_lookup.clear();
        state.last_resolved_cid = None;
        self.flush_account_reset(state, None, host)
    }

    pub(super) fn commit_session(
        &mut self,
        state: &mut WindowState,
        mut next_session: StoredSession,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        self.init_secret_op = None;
        self.account_epoch = self.account_epoch.wrapping_add(1);
        state.account_reset = false;
        for other in self.windows.values_mut() {
            other.account_reset = true;
        }
        if next_session.buvid3.is_empty() {
            next_session.buvid3 = std::mem::take(&mut self.session.buvid3);
        }
        self.session = next_session;
        self.cid_lookup.clear();
        let requests: BTreeSet<_> = state
            .http_jobs
            .values()
            .filter_map(|job| job.request_id)
            .chain(state.pending_wbi_browse.iter().map(|p| p.request_id))
            .chain(state.pending_wbi_playurl.iter().map(|p| p.request_id))
            .collect();
        state.clear_scoped(host);
        state.qr = None;
        state.browse_seen.clear();
        state.browse_cursors.clear();
        state.browse_page = None;
        state.last_resolved_cid = None;
        requests
            .into_iter()
            .map(|request_id| HostCommand::SourceError {
                request_id,
                message: "账号已切换，请刷新目录重试".into(),
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
        // Retire handles only in their owning window, including newly delivered
        // resources that did not exist when another window logged out or switched accounts.
        if let Some(HostEvent::Completed {
            operation_id,
            result,
            ..
        }) = event
        {
            if let Some(job) = state.http_jobs.get(operation_id) {
                if job.stream.is_none() {
                    if let Ok(HttpResult::Response { stream_handle, .. }) =
                        completed(result.clone())
                    {
                        host.close(stream_handle);
                    }
                } else if let Ok(read) = completed::<ReadResult>(result.clone()) {
                    host.close(read.blob);
                }
            } else if let Some(job) = state.sub_jobs.get(operation_id) {
                if job.writer.is_none() {
                    if let Ok(opened) = completed::<ArtifactWriterResult>(result.clone()) {
                        host.close(opened.artifact_writer);
                    }
                } else if let Ok(artifact) = completed::<ArtifactResult>(result.clone()) {
                    host.close(artifact.artifact);
                }
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
            .chain(state.pending_wbi_browse.iter().map(|p| p.request_id))
            .chain(state.pending_wbi_playurl.iter().map(|p| p.request_id))
            .filter(|id| Some(*id) != cancelled)
            .collect();
        state.clear_scoped(host);
        state.qr = None;
        state.browse_seen.clear();
        state.browse_cursors.clear();
        state.browse_page = None;
        state.last_resolved_cid = None;
        let message = if self.session.is_authenticated() {
            "账号已切换，请刷新目录重试"
        } else {
            "账号已退出，请重新登录或刷新目录"
        };
        let mut commands: Vec<_> = requests
            .into_iter()
            .map(|request_id| HostCommand::SourceError {
                request_id,
                message: message.into(),
            })
            .collect();
        commands.extend(self.sync_ui(state));
        commands.extend(ui::refresh_all_sources());
        commands
    }
}
