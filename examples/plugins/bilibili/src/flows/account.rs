//! Account flows (`Nav` WBI refresh, QR code login generation and polling) for `community.bilibili`.

use crate::{
    BilibiliPlugin, HttpFlow, WindowState,
    auth::{self, QrPollOutcome},
    ui,
};
use iris_plugin_sdk::{
    api::v2::{
        HostCommand,
        task::{CredentialBucket, Task},
    },
    client::Host,
    error_message,
    serde_json::{self, Value},
};

impl BilibiliPlugin {
    pub(super) fn finish_account_json_flow(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        json: &Value,
        headers: &[(String, String)],
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
            HttpFlow::Nav {
                account_epoch,
                open_modal,
            } => {
                let prev_mid = self.session.dede_user_id.clone();
                if account_epoch == self.account_epoch {
                    auth::apply_nav_response(json, &mut self.session, &mut self.wbi, now_secs);
                } else {
                    let mut ignored = auth::StoredSession::default();
                    auth::apply_nav_response(json, &mut ignored, &mut self.wbi, now_secs);
                }
                let mid_resolved = account_epoch == self.account_epoch
                    && self.session.is_authenticated()
                    && prev_mid != self.session.dede_user_id
                    && !self.session.dede_user_id.is_empty();
                if mid_resolved {
                    state.submit_background_secret(&self.session, self.account_epoch, host);
                }
                let mut cmds = Vec::new();
                let mut replayed_sources = std::collections::BTreeSet::new();
                let pending = std::mem::take(&mut state.pending_wbi_browse);
                for item in pending {
                    replayed_sources.insert(item.source_id.clone());
                    cmds.extend(self.begin_browse(
                        state,
                        item.request_id,
                        item.source_id,
                        item.folder_id,
                        item.cursor,
                        item.query,
                        item.filters,
                        true,
                        host,
                        now_secs,
                    ));
                }
                if mid_resolved {
                    for sid in ["favorites", "bangumi"] {
                        if !replayed_sources.contains(sid) {
                            cmds.push(HostCommand::RefreshSource {
                                source_id: sid.into(),
                            });
                        }
                    }
                }
                let pending_playurl = std::mem::take(&mut state.pending_wbi_playurl);
                for item in pending_playurl {
                    if self.wbi.mixin().is_some() {
                        cmds.extend(self.request_vod_playurl(
                            state,
                            item.request_id,
                            item.requested_entry_id,
                            item.meta,
                            item.variant,
                            true,
                            host,
                            now_secs,
                        ));
                    } else {
                        cmds.push(HostCommand::SourceError {
                            request_id: item.request_id,
                            message: "获取 B站 WBI 签名密钥失败，请稍后重试".into(),
                        });
                    }
                }
                if state.needs_wbi_player_v2 && self.wbi.mixin().is_some() {
                    state.needs_wbi_player_v2 = false;
                    if let (Some(media), Some(cid)) = (state.active_media, state.active_cid) {
                        let (bvid, aid) = (state.active_bvid.clone(), state.active_aid);
                        cmds.extend(self.request_player_v2(
                            state, media, &bvid, aid, cid, true, host, now_secs,
                        ));
                    }
                }
                cmds.extend(self.sync_ui(state));
                if open_modal {
                    cmds.push(HostCommand::OpenModal {
                        modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                    });
                }
                cmds
            }
            HttpFlow::QrGenerate { open_modal } => match auth::parse_qr_generate(json) {
                Ok(qr) => {
                    state.qr = Some(qr);
                    let mut cmds = self.sync_ui(state);
                    if open_modal {
                        cmds.push(HostCommand::OpenModal {
                            modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                        });
                    }
                    cmds
                }
                Err(msg) => vec![ui::toast(msg)],
            },
            HttpFlow::QrPoll {
                account_epoch,
                open_modal,
                manual,
            } => {
                if account_epoch != self.account_epoch {
                    return Vec::new();
                }
                match auth::parse_qr_poll(json, headers, &self.session.buvid3) {
                    Ok(QrPollOutcome::Waiting(hint)) => {
                        let changed = state.qr.as_ref().is_some_and(|qr| qr.status_hint != hint);
                        if let Some(qr) = &mut state.qr {
                            qr.status_hint = hint.clone();
                        }
                        let mut cmds = Vec::new();
                        if manual || changed {
                            cmds.extend(self.sync_ui(state));
                            if open_modal {
                                cmds.push(HostCommand::OpenModal {
                                    modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                                });
                            }
                        }
                        if manual {
                            cmds.push(ui::toast(hint));
                        }
                        cmds
                    }
                    Ok(QrPollOutcome::Expired(hint)) => {
                        state.qr = None;
                        let mut cmds = self.sync_ui(state);
                        if open_modal {
                            cmds.push(HostCommand::OpenModal {
                                modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                            });
                        }
                        cmds.push(ui::toast(hint));
                        cmds
                    }
                    Ok(QrPollOutcome::Success(next_session)) => {
                        let Ok(encoded) = serde_json::to_string(&next_session) else {
                            return vec![ui::toast("序列化登录会话失败")];
                        };
                        match host.submit(&Task::SecretWrite {
                            bucket: CredentialBucket::Plugin,
                            key: "bili.session".into(),
                            value: Some(encoded),
                        }) {
                            Ok(op) => {
                                state.set_secret_write(
                                    op,
                                    self.account_epoch,
                                    next_session,
                                    true,
                                    host,
                                );
                                Vec::new()
                            }
                            Err(e) => {
                                vec![ui::toast(format!("保存扫码凭据失败：{}", error_message(e)))]
                            }
                        }
                    }
                    Err(msg) => {
                        if manual {
                            vec![ui::toast(msg)]
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            _ => Vec::new(),
        }
    }
}
