//! Sidebar/command actions and login modal submission handlers for `community.baidupan`.

use crate::{
    BaiduPanPlugin, HttpFlow, OAuthRetry, WindowState,
    auth::{self, StoredSession},
    sign, ui,
};
use iris_plugin_sdk::{
    api::v2::{ErrorCode, HostCommand},
    client::Host,
    error_message,
};
use std::collections::BTreeMap;

impl BaiduPanPlugin {
    pub(crate) fn handle_action(
        &mut self,
        state: &mut WindowState,
        id: &str,
        value: Option<String>,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let mut cmds = Vec::new();
        match id {
            "login" => {
                cmds.push(HostCommand::OpenModal {
                    modal: Box::new(ui::build_login_modal(state.qr.as_ref())),
                });
            }
            "start_qr" | "modal_start_qr" => {
                let open_modal = id == "modal_start_qr";
                let mut entropy = [0x5Au8; 16];
                let _ = host.random(&mut entropy);
                let gid = sign::generate_gid(&entropy);
                let url = format!(
                    "https://passport.baidu.com/v2/api/getqrcode?lp=pc&qrloginfrom=pc&gid={gid}&callback=tangram_guid_{now_ms}&apiver=v3&tt={now_ms}&tpl=netdisk&_={now_ms}"
                );
                match self.start_http(
                    state,
                    "GET",
                    url,
                    vec![("User-Agent".into(), auth::WEB_USER_AGENT.into())],
                    true,
                    Some(20_000),
                    None,
                    HttpFlow::QrGetSign { open_modal },
                    host,
                ) {
                    Ok(()) => cmds.push(ui::toast("正在获取百度网盘登录二维码…")),
                    Err(e) => cmds.push(ui::toast(format!("获取二维码失败：{}", error_message(e)))),
                }
            }
            "poll_qr" | "modal_poll_qr" | "auto_poll_qr" | "auto_modal_poll_qr" => {
                let manual = matches!(id, "poll_qr" | "modal_poll_qr");
                let open_modal = matches!(id, "modal_poll_qr" | "auto_modal_poll_qr");
                let in_flight = state.http_jobs.values().any(|j| {
                    matches!(
                        j.flow,
                        HttpFlow::QrPoll { .. } | HttpFlow::QrConfirmLogin { .. }
                    )
                });
                if let Some(qr) = &state.qr {
                    if !in_flight {
                        let url = format!(
                            "https://passport.baidu.com/channel/unicast?channel_id={}&tpl=netdisk&apiver=v3&tt={now_ms}",
                            sign::encode_query_component(&qr.sign)
                        );
                        let mut headers = vec![("User-Agent".into(), auth::WEB_USER_AGENT.into())];
                        if !qr.baiduid.is_empty() {
                            headers.push(("Cookie".into(), format!("BAIDUID={}", qr.baiduid)));
                        }
                        let timeout = if manual { Some(10_000) } else { Some(15_000) };
                        if let Err(e) = self.start_http(
                            state,
                            "GET",
                            url,
                            headers,
                            true,
                            timeout,
                            None,
                            HttpFlow::QrPoll { open_modal, manual },
                            host,
                        ) && manual
                        {
                            cmds.push(ui::toast(format!("检查扫码状态失败：{}", error_message(e))));
                        }
                    }
                } else if manual {
                    cmds.push(ui::toast("请先生成百度网盘登录二维码"));
                }
            }
            "refresh_account" => {
                if !self.session.is_authenticated() {
                    cmds.push(ui::toast("尚未登录百度网盘"));
                } else if let Err(e) = self.request_user_info(state, None, false, true, host) {
                    cmds.push(ui::toast(format!("刷新账号信息失败：{}", error_message(e))));
                } else {
                    cmds.push(ui::toast("正在刷新百度网盘账号与空间配额…"));
                }
            }
            "logout" => {
                cmds.extend(self.reset_account(state, host));
                state.submit_logout_secret(self.account_epoch, host);
                cmds.extend(self.maybe_sync_sources());
                cmds.push(ui::toast("已退出百度网盘账号"));
            }
            "set_playback_mode" => {
                if let Some(mode) = value.filter(|m| {
                    matches!(
                        m.as_str(),
                        "direct" | "transcode_1080" | "transcode_720" | "transcode_480"
                    )
                }) {
                    self.prefs.playback_mode = mode;
                    self.persist_prefs(host);
                    cmds.extend(self.sync_ui(state));
                    cmds.push(ui::toast("已更新视频默认播放线路"));
                }
            }
            "set_openapi_link_api" => {
                if let Some(api) =
                    value.filter(|v| matches!(v.as_str(), "official" | "crack" | "crack_video"))
                {
                    self.prefs.openapi_link_api = api;
                    self.persist_prefs(host);
                    cmds.extend(self.sync_ui(state));
                    cmds.push(ui::toast("已更新开放平台取链策略"));
                }
            }
            "set_auto_subtitles" => {
                self.prefs.auto_subtitles = match value.as_deref() {
                    Some("true") => true,
                    Some("false") => false,
                    _ => !self.prefs.auto_subtitles,
                };
                self.persist_prefs(host);
                cmds.extend(self.sync_ui(state));
            }
            "set_show_thumbnails" => {
                self.prefs.show_thumbnails = match value.as_deref() {
                    Some("true") => true,
                    Some("false") => false,
                    _ => !self.prefs.show_thumbnails,
                };
                self.persist_prefs(host);
                cmds.extend(self.sync_ui(state));
                cmds.extend(ui::refresh_all_sources());
            }
            "set_default_order" => {
                if let Some(ord) = value.filter(|v| matches!(v.as_str(), "name" | "time" | "size"))
                {
                    self.prefs.default_order = ord;
                    self.persist_prefs(host);
                    cmds.extend(self.sync_ui(state));
                    cmds.extend(ui::refresh_all_sources());
                }
            }
            _ => {}
        }
        Ok(cmds)
    }

    pub(crate) fn handle_modal_submit(
        &mut self,
        state: &mut WindowState,
        modal_id: &str,
        values: &BTreeMap<String, String>,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        if modal_id != "baidupan.login" {
            return Ok(Vec::new());
        }
        let cookie_raw = values
            .get("cookie")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let at_raw = values
            .get("access_token")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let rt_raw = values
            .get("refresh_token")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let cid_raw = values
            .get("client_id")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let csec_raw = values
            .get("client_secret")
            .map(String::as_str)
            .unwrap_or("")
            .trim();

        let candidate = if !cookie_raw.is_empty() {
            match StoredSession::parse_cookie_input(cookie_raw) {
                Ok(s) => s,
                Err(msg) => return Ok(vec![ui::toast(msg)]),
            }
        } else if !at_raw.is_empty() || !rt_raw.is_empty() {
            match StoredSession::parse_openapi_input(at_raw, rt_raw, cid_raw, csec_raw) {
                Ok(s) => s,
                Err(msg) => return Ok(vec![ui::toast(msg)]),
            }
        } else {
            return Ok(vec![ui::toast(
                "请先扫码登录，或在下方粘贴 Cookie (BDUSS) 或开放平台令牌",
            )]);
        };

        if candidate.mode == auth::AuthMode::OpenApi && candidate.access_token.is_empty() {
            let retry = OAuthRetry::VerifyLogin {
                close_modal: true,
                notify_on_success: true,
                save_after_verify: true,
            };
            return match self.start_oauth_refresh(state, &candidate, retry, host) {
                Ok(()) => Ok(vec![ui::toast("正在通过 refresh_token 换取访问令牌…")]),
                Err(e) => Ok(vec![ui::toast(format!(
                    "刷新 OAuth 令牌失败：{}",
                    error_message(e)
                ))]),
            };
        }

        match self.request_user_info(state, Some(candidate), true, true, host) {
            Ok(()) => Ok(vec![ui::toast("正在校验百度网盘凭据…")]),
            Err(e) => Ok(vec![ui::toast(format!(
                "无法校验凭据：{}",
                error_message(e)
            ))]),
        }
    }
}
