//! QR code login, user profile verification, storage quota, and OAuth refresh flows.

use super::super::*;
use crate::{
    auth::{AuthMode, BAIDU_APP_ID, QrPollOutcome, StoredSession},
    sign::{build_query, encode_query_component},
};
use iris_plugin_sdk::api::v2::task::{CredentialBucket, Task};

impl BaiduPanPlugin {
    pub(crate) fn request_user_info(
        &self,
        state: &mut WindowState,
        session_override: Option<StoredSession>,
        close_modal: bool,
        notify_on_success: bool,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        let target_session = session_override.as_ref().unwrap_or(&self.session);
        let (url, headers) = match target_session.mode {
            AuthMode::Cookie => (
                format!(
                    "https://pan.baidu.com/rest/2.0/membership/user/info?method=query&clienttype=0&app_id={BAIDU_APP_ID}&web=1"
                ),
                target_session.web_api_headers(),
            ),
            AuthMode::OpenApi => (
                format!(
                    "https://pan.baidu.com/rest/2.0/xpan/nas?method=uinfo&access_token={}",
                    encode_query_component(&target_session.access_token)
                ),
                target_session.web_api_headers(),
            ),
        };
        self.start_http(
            state,
            "GET",
            url,
            headers,
            true,
            Some(20_000),
            None,
            HttpFlow::UserInfo {
                pending_session: session_override.map(Box::new),
                close_modal,
                notify_on_success,
            },
            host,
        )
    }

    pub(crate) fn request_quota(&self, state: &mut WindowState, host: &mut impl Host) {
        if !self.session.is_authenticated() {
            return;
        }
        let url = match self.session.mode {
            AuthMode::Cookie => {
                "https://pan.baidu.com/api/quota?checkfree=1&checkexpire=1".to_owned()
            }
            AuthMode::OpenApi => format!(
                "https://pan.baidu.com/api/quota?checkfree=1&checkexpire=1&access_token={}",
                encode_query_component(&self.session.access_token)
            ),
        };
        let _ = self.start_http(
            state,
            "GET",
            url,
            self.session.web_api_headers(),
            true,
            Some(20_000),
            None,
            HttpFlow::Quota,
            host,
        );
    }

    pub(crate) fn start_oauth_refresh(
        &self,
        state: &mut WindowState,
        source_session: &StoredSession,
        retry: OAuthRetry,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        let params = vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", source_session.refresh_token.clone()),
            ("client_id", source_session.client_id.clone()),
            ("client_secret", source_session.client_secret.clone()),
        ];
        let url = format!(
            "https://openapi.baidu.com/oauth/2.0/token?{}",
            build_query(&params)
        );
        self.start_http(
            state,
            "GET",
            url,
            vec![("User-Agent".into(), auth::WEB_USER_AGENT.into())],
            true,
            Some(20_000),
            retry.request_id(),
            HttpFlow::OAuthRefresh {
                base_session: Box::new(source_session.clone()),
                retry: Box::new(retry),
            },
            host,
        )
    }

    pub(crate) fn persist_prefs(&self, host: &mut impl Host) {
        if let Ok(raw) = serde_json::to_string(&self.prefs) {
            let _ = host.submit(&Task::StateWrite {
                key: "prefs".into(),
                value: Some(raw),
            });
        }
    }

    pub(crate) fn save_session(
        &self,
        state: &mut WindowState,
        next_session: StoredSession,
        close_modal: bool,
        notify_login: bool,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        let Ok(encoded) = serde_json::to_string(&next_session) else {
            return vec![ui::toast("序列化登录凭据失败")];
        };
        match host.submit(&Task::SecretWrite {
            bucket: CredentialBucket::Plugin,
            key: SECRET_KEY.into(),
            value: Some(encoded),
        }) {
            Ok(op) => {
                state.secret_write_op = Some((op, next_session, close_modal, notify_login));
                Vec::new()
            }
            Err(e) => vec![ui::toast(format!("保存凭据失败：{e:?}"))],
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_account_flow(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        headers: &[(String, String)],
        body: &[u8],
        host: &mut impl Host,
        now_ms: u64,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
            HttpFlow::QrGetSign { open_modal } => {
                let text = String::from_utf8_lossy(body);
                match auth::parse_getqrcode(&text, headers) {
                    Ok((sign_val, img_url, baiduid)) => {
                        let mut hdrs = vec![("User-Agent".into(), auth::WEB_USER_AGENT.into())];
                        if !baiduid.is_empty() {
                            hdrs.push(("Cookie".into(), format!("BAIDUID={baiduid}")));
                        }
                        if let Err(e) = self.start_http(
                            state,
                            "GET",
                            img_url.clone(),
                            hdrs,
                            true,
                            Some(20_000),
                            None,
                            HttpFlow::QrDownloadPng {
                                sign: sign_val,
                                baiduid,
                                img_url,
                                open_modal,
                            },
                            host,
                        ) {
                            vec![ui::toast(format!("下载二维码图片失败：{e:?}"))]
                        } else {
                            Vec::new()
                        }
                    }
                    Err(msg) => vec![ui::toast(msg)],
                }
            }
            HttpFlow::QrDownloadPng {
                sign: qr_sign,
                baiduid,
                img_url,
                open_modal,
            } => match qr_png::decode_qr_png_to_matrix(body) {
                Ok(matrix) => {
                    state.qr = Some(auth::PendingQr {
                        sign: qr_sign,
                        baiduid,
                        img_url,
                        matrix,
                        status_hint: "请使用手机「百度网盘 App」扫一扫并确认登录（将自动检测状态）"
                            .into(),
                    });
                    let mut cmds = self.sync_ui(state);
                    if open_modal {
                        cmds.push(HostCommand::OpenModal {
                            modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                        });
                    }
                    cmds
                }
                Err(msg) => vec![ui::toast(format!("解析百度二维码失败：{msg}"))],
            },
            HttpFlow::QrPoll { open_modal, manual } => {
                let text = String::from_utf8_lossy(body);
                match auth::parse_unicast_poll(&text) {
                    Ok(QrPollOutcome::Waiting(hint)) | Ok(QrPollOutcome::Scanned(hint)) => {
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
                    Ok(QrPollOutcome::Confirmed { v_code }) => {
                        let baiduid = state
                            .qr
                            .as_ref()
                            .map(|q| q.baiduid.clone())
                            .unwrap_or_default();
                        let redirect = encode_query_component("https://pan.baidu.com/disk/main");
                        let url = format!(
                            "https://passport.baidu.com/v3/login/main/qrbdusslogin?v={now_ms}&bduss={}&u={redirect}&tpl=netdisk&qrcode=1&apiver=v3&tt={now_ms}",
                            encode_query_component(&v_code)
                        );
                        let mut hdrs = vec![("User-Agent".into(), auth::WEB_USER_AGENT.into())];
                        if !baiduid.is_empty() {
                            hdrs.push(("Cookie".into(), format!("BAIDUID={baiduid}")));
                        }
                        if let Err(e) = self.start_http(
                            state,
                            "GET",
                            url,
                            hdrs,
                            false,
                            Some(20_000),
                            None,
                            HttpFlow::QrConfirmLogin {
                                baiduid,
                                open_modal,
                            },
                            host,
                        ) {
                            vec![ui::toast(format!("确认扫码登录失败：{e:?}"))]
                        } else {
                            Vec::new()
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
            HttpFlow::QrConfirmLogin {
                baiduid,
                open_modal,
            } => match auth::extract_qrbdusslogin_session(headers, &baiduid) {
                Ok(next_session) => {
                    if self
                        .request_user_info(
                            state,
                            Some(next_session.clone()),
                            open_modal,
                            true,
                            host,
                        )
                        .is_err()
                    {
                        self.save_session(state, next_session, open_modal, true, host)
                    } else {
                        Vec::new()
                    }
                }
                Err(msg) => vec![ui::toast(msg)],
            },
            HttpFlow::UserInfo {
                pending_session,
                close_modal,
                notify_on_success,
            } => self.finish_user_info(
                state,
                pending_session,
                close_modal,
                notify_on_success,
                body,
                host,
            ),
            HttpFlow::Quota => {
                if let Ok(json) = serde_json::from_slice::<Value>(body)
                    && let Some(q) = auth::parse_quota(&json)
                {
                    self.quota = Some(q);
                    return self.sync_ui(state);
                }
                Vec::new()
            }
            HttpFlow::OAuthRefresh {
                base_session,
                retry,
            } => self.finish_oauth_refresh(
                state,
                *base_session,
                *retry,
                body,
                host,
                now_ms,
                now_secs,
            ),
            _ => Vec::new(),
        }
    }

    fn finish_user_info(
        &mut self,
        state: &mut WindowState,
        pending_session: Option<Box<StoredSession>>,
        close_modal: bool,
        notify_on_success: bool,
        body: &[u8],
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        let Ok(json) = serde_json::from_slice::<Value>(body) else {
            return if notify_on_success {
                vec![ui::toast("账号校验响应不是有效 JSON")]
            } else {
                Vec::new()
            };
        };
        let mut target = pending_session
            .as_deref()
            .cloned()
            .unwrap_or_else(|| self.session.clone());
        let res = match target.mode {
            AuthMode::Cookie => auth::apply_membership_uinfo(&json, &mut target).map(|q| {
                if let Some(quota) = q {
                    self.quota = Some(quota);
                }
            }),
            AuthMode::OpenApi => auth::apply_openapi_uinfo(&json, &mut target),
        };
        match res {
            Ok(()) => {
                if pending_session.is_some() {
                    self.save_session(state, target, close_modal, notify_on_success, host)
                } else {
                    let changed = self.session.uid != target.uid
                        || self.session.username != target.username
                        || self.session.vip_type != target.vip_type;
                    self.session = target;
                    if changed && let Ok(raw) = serde_json::to_string(&self.session) {
                        let _ = host.submit(&Task::SecretWrite {
                            bucket: CredentialBucket::Plugin,
                            key: SECRET_KEY.into(),
                            value: Some(raw),
                        });
                    }
                    self.request_quota(state, host);
                    let mut cmds = self.sync_ui(state);
                    if notify_on_success {
                        cmds.push(ui::toast("已刷新百度网盘账号信息"));
                    }
                    cmds
                }
            }
            Err(msg) => {
                let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
                if target.can_refresh_oauth() && auth::is_token_expired_errno(errno) {
                    let retry = OAuthRetry::VerifyLogin {
                        close_modal,
                        notify_on_success,
                        save_after_verify: pending_session.is_some(),
                    };
                    if self
                        .start_oauth_refresh(state, &target, retry, host)
                        .is_ok()
                    {
                        return Vec::new();
                    }
                }
                if notify_on_success {
                    vec![ui::toast(msg)]
                } else {
                    Vec::new()
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_oauth_refresh(
        &mut self,
        state: &mut WindowState,
        mut base_session: StoredSession,
        retry: OAuthRetry,
        body: &[u8],
        host: &mut impl Host,
        now_ms: u64,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let Ok(json) = serde_json::from_slice::<Value>(body) else {
            return fail_oauth_retry(&retry, "刷新 OAuth 令牌响应格式错误");
        };
        match auth::parse_oauth_refresh(&json) {
            Ok((new_at, new_rt)) => {
                base_session.access_token = new_at;
                if !new_rt.is_empty() {
                    base_session.refresh_token = new_rt;
                }
                match retry {
                    OAuthRetry::VerifyLogin {
                        close_modal,
                        notify_on_success,
                        save_after_verify,
                    } => {
                        let override_sess = save_after_verify.then(|| base_session.clone());
                        if !save_after_verify {
                            self.session = base_session;
                            persist_secret(&self.session, host);
                        }
                        if let Err(e) = self.request_user_info(
                            state,
                            override_sess,
                            close_modal,
                            notify_on_success,
                            host,
                        ) {
                            vec![ui::toast(format!("校验刷新后的令牌失败：{e:?}"))]
                        } else {
                            Vec::new()
                        }
                    }
                    OAuthRetry::Browse {
                        request_id,
                        source_id,
                        folder_id,
                        cursor,
                        query,
                        filters,
                    } => {
                        self.session = base_session;
                        persist_secret(&self.session, host);
                        self.begin_browse(
                            state, request_id, source_id, folder_id, cursor, query, filters, true,
                            host, now_ms,
                        )
                    }
                    OAuthRetry::Resolve {
                        request_id,
                        source_id,
                        entry_id,
                        variant,
                    } => {
                        self.session = base_session;
                        persist_secret(&self.session, host);
                        self.begin_resolve(
                            state, request_id, &source_id, &entry_id, variant, true, host, now_secs,
                        )
                    }
                }
            }
            Err(msg) => fail_oauth_retry(&retry, &msg),
        }
    }
}

fn persist_secret(session: &StoredSession, host: &mut impl Host) {
    if let Ok(raw) = serde_json::to_string(session) {
        let _ = host.submit(&Task::SecretWrite {
            bucket: CredentialBucket::Plugin,
            key: SECRET_KEY.into(),
            value: Some(raw),
        });
    }
}

fn fail_oauth_retry(retry: &OAuthRetry, msg: &str) -> Vec<HostCommand> {
    if let Some(request_id) = retry.request_id() {
        vec![HostCommand::SourceError {
            request_id,
            message: msg.to_owned(),
        }]
    } else {
        vec![ui::toast(msg)]
    }
}
