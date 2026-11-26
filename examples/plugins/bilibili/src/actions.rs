//! Sidebar/command actions and modal submission handlers for `community.bilibili`.

use crate::{BilibiliPlugin, HttpFlow, WindowState, auth::StoredSession, playback, ui, wbi};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, HostCommand,
        task::{CredentialBucket, Task},
    },
    client::Host,
    error_message, serde_json,
};
use std::collections::BTreeMap;

impl BilibiliPlugin {
    pub(crate) fn handle_action(
        &mut self,
        state: &mut WindowState,
        id: &str,
        value: Option<String>,
        host: &mut impl Host,
        now_secs: u64,
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
                let flow = HttpFlow::QrGenerate { open_modal };
                match self.start_http(
                    state,
                    "GET",
                    "https://passport.bilibili.com/x/passport-login/web/qrcode/generate".into(),
                    true,
                    None,
                    HttpFlow::QrGenerate { open_modal },
                    host,
                ) {
                    Ok(()) => cmds.push(ui::toast("正在获取哔哩哔哩扫码登录二维码…")),
                    Err(e) => cmds.extend(self.fail_http_flow(state, &flow, e)),
                }
            }
            "poll_qr" | "modal_poll_qr" | "auto_poll_qr" | "auto_modal_poll_qr" => {
                let manual = matches!(id, "poll_qr" | "modal_poll_qr");
                let open_modal = matches!(id, "modal_poll_qr" | "auto_modal_poll_qr");
                let in_flight = state
                    .http_jobs
                    .values()
                    .any(|j| matches!(j.flow, HttpFlow::QrPoll { .. }));
                if let Some(qr) = &state.qr {
                    if !in_flight {
                        let url = format!(
                            "https://passport.bilibili.com/x/passport-login/web/qrcode/poll?qrcode_key={}",
                            wbi::encode_query_component(&qr.qrcode_key)
                        );
                        let account_epoch = self.account_epoch;
                        if let Err(e) = self.start_http(
                            state,
                            "GET",
                            url,
                            false,
                            None,
                            HttpFlow::QrPoll {
                                account_epoch,
                                open_modal,
                                manual,
                            },
                            host,
                        ) && manual
                        {
                            cmds.extend(self.fail_http_flow(
                                state,
                                &HttpFlow::QrPoll {
                                    account_epoch,
                                    open_modal,
                                    manual,
                                },
                                e,
                            ));
                        }
                    }
                } else if manual {
                    cmds.push(ui::toast("请先生成登录二维码"));
                }
            }
            "logout" => {
                cmds.extend(self.reset_account(state, host));
                state.submit_logout_secret(self.account_epoch, host);
                cmds.extend(self.maybe_sync_sources());
                cmds.push(ui::toast("已退出哔哩哔哩账号"));
            }
            "open_target" => {
                cmds.push(HostCommand::OpenModal {
                    modal: Box::new(ui::build_target_modal()),
                });
            }
            "open_quick_target" => {
                let raw = value.unwrap_or_default();
                if let Some(url) = wbi::canonical_target_url(&raw) {
                    cmds.push(ui::toast(format!(
                        "可通过「打开网络地址 (Ctrl+U)」输入 {url} 直接播放，或在源搜索框检索"
                    )));
                } else {
                    cmds.push(ui::toast("未识别有效的 BV / av / ep / 直播间号"));
                }
            }
            "toggle_danmaku" | "set_danmaku_enabled" => {
                self.prefs.danmaku_enabled = match value.as_deref() {
                    Some("true") => true,
                    Some("false") => false,
                    _ => !self.prefs.danmaku_enabled,
                };
                self.persist_prefs(host);
                if self.prefs.danmaku_enabled {
                    cmds.extend(self.push_danmaku_window(state, host));
                    cmds.push(ui::toast("哔哩哔哩弹幕已开启"));
                } else if let Some(media) = state.active_media {
                    cmds.push(HostCommand::ClearTimedOverlay {
                        media,
                        generation: state.overlay_gen,
                        track: "danmaku".into(),
                    });
                    cmds.push(ui::toast("哔哩哔哩弹幕已关闭"));
                }
                cmds.extend(self.sync_ui(state));
            }
            "set_danmaku_opacity" => {
                if let Some(v) = value.and_then(|s| s.parse::<f64>().ok()) {
                    self.prefs.danmaku_opacity = (v.round() as i32).clamp(20, 100) as u8;
                    self.persist_prefs(host);
                    cmds.extend(self.push_danmaku_window(state, host));
                    cmds.extend(self.sync_ui(state));
                }
            }
            "set_preferred_qn" => {
                if let Some(qn) = value.and_then(|s| s.parse::<u32>().ok()) {
                    self.prefs.preferred_qn = qn;
                    self.persist_prefs(host);
                    cmds.extend(self.sync_ui(state));
                    cmds.push(ui::toast(format!(
                        "默认最高画质已设为 {}",
                        playback::qn_label(qn)
                    )));
                }
            }
            "set_preferred_codec" => {
                if let Some(c) = value.filter(|c| matches!(c.as_str(), "hevc" | "av1" | "avc")) {
                    self.prefs.preferred_codec = c.clone();
                    self.persist_prefs(host);
                    cmds.extend(self.sync_ui(state));
                    cmds.push(ui::toast(format!(
                        "首选视频编码已设为 {}",
                        c.to_uppercase()
                    )));
                }
            }
            "reload_danmaku" => {
                if let (Some(media), Some(cid)) = (state.active_media, state.active_cid) {
                    let bvid = state.active_bvid.clone();
                    let aid = state.active_aid;
                    cmds.extend(
                        self.fetch_enhancements(state, media, &bvid, aid, cid, host, now_secs),
                    );
                    cmds.push(ui::toast("正在刷新当前视频弹幕与字幕…"));
                } else {
                    cmds.push(ui::toast("当前未在播放哔哩哔哩点播视频"));
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
        match modal_id {
            "bili.login" => {
                let raw = values.get("cookie").map(String::as_str).unwrap_or("");
                let parsed = match StoredSession::parse_cookie_input(raw, &self.session.buvid3) {
                    Ok(s) => s,
                    Err(msg) => return Ok(vec![ui::toast(msg)]),
                };
                let encoded = serde_json::to_string(&parsed).map_err(|_| ErrorCode::Internal)?;
                match host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: "bili.session".into(),
                    value: Some(encoded),
                }) {
                    Ok(op) => {
                        state.set_secret_write(op, self.account_epoch, parsed, true, host);
                        Ok(Vec::new())
                    }
                    Err(e) => Ok(vec![ui::toast(format!(
                        "保存凭据失败：{}",
                        error_message(e)
                    ))]),
                }
            }
            "bili.open_target" => {
                let target = values
                    .get("target")
                    .map(String::as_str)
                    .unwrap_or("")
                    .trim();
                let Some(url) = wbi::canonical_target_url(target) else {
                    return Ok(vec![ui::toast(
                        "请输入有效的 BV / av / ep / 直播间号或链接",
                    )]);
                };
                Ok(vec![
                    HostCommand::CloseModal {
                        id: Some("bili.open_target".into()),
                    },
                    ui::toast(format!(
                        "可通过「打开网络地址 (Ctrl+U)」输入 {url} 直接播放，或在源搜索框检索 {target}"
                    )),
                ])
            }
            _ => Ok(Vec::new()),
        }
    }
}
