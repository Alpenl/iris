//! Baidu Netdisk (`community.baidupan`) Wasm sandbox plugin for Iris (Protocol 2 / ABI 2),
//! referencing `komorebiCarry/BaiduPCS-Rust` (QR login, Cookie BDUSS/STOKEN, PCS Locate v4)
//! and `OpenListTeam/OpenList` (Open Platform OAuth, `filemetas` / DLNA `dlink` / `streaming`).

pub mod auth;
pub mod catalog;
mod flows;
pub mod media;
pub mod playback;
pub mod qr_png;
pub mod sign;
pub mod ui;

use auth::{PendingQr, Preferences, QuotaInfo, StoredSession};
use catalog::{BrowseKind, CachedSubtitle, FolderCache};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, Generation, HostCommand, HostEvent, Id,
        task::{CredentialBucket, Task},
        task_result::StoreRead,
    },
    client::{Host, completed},
    serde_json::{self, Value},
};
use playback::{OpenApiLinkKind, ResolveTarget};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const SECRET_KEY: &str = "baidupan.session";

pub(crate) enum OAuthRetry {
    VerifyLogin {
        close_modal: bool,
        notify_on_success: bool,
        save_after_verify: bool,
    },
    Browse {
        request_id: Id,
        source_id: String,
        folder_id: Option<String>,
        cursor: Option<String>,
        query: Option<String>,
        filters: BTreeMap<String, String>,
    },
    Resolve {
        request_id: Id,
        source_id: String,
        entry_id: String,
        variant: Option<String>,
    },
}

impl OAuthRetry {
    pub(crate) fn request_id(&self) -> Option<Id> {
        match self {
            Self::Browse { request_id, .. } | Self::Resolve { request_id, .. } => Some(*request_id),
            Self::VerifyLogin { .. } => None,
        }
    }
}

pub(crate) enum HttpFlow {
    QrGetSign {
        open_modal: bool,
    },
    QrDownloadPng {
        sign: String,
        baiduid: String,
        img_url: String,
        open_modal: bool,
    },
    QrPoll {
        open_modal: bool,
        manual: bool,
    },
    QrConfirmLogin {
        baiduid: String,
        open_modal: bool,
    },
    UserInfo {
        pending_session: Option<Box<StoredSession>>,
        close_modal: bool,
        notify_on_success: bool,
    },
    Quota,
    OAuthRefresh {
        base_session: Box<StoredSession>,
        retry: Box<OAuthRetry>,
    },
    Browse {
        request_id: Id,
        source_id: String,
        kind: BrowseKind,
        folder_id: Option<String>,
        cursor: Option<String>,
        query: Option<String>,
        filters: BTreeMap<String, String>,
        retried_oauth: bool,
    },
    ResolveLocate {
        request_id: Id,
        target: Box<ResolveTarget>,
        preferred_line: usize,
        allow_hls_fallback: bool,
    },
    ResolveOpenApiMeta {
        request_id: Id,
        target: Box<ResolveTarget>,
        preferred_line: usize,
        api_kind: OpenApiLinkKind,
        retried_oauth: bool,
    },
    ResolveOpenApiHead {
        request_id: Id,
        target: Box<ResolveTarget>,
        preferred_line: usize,
        fallback_dlink: String,
    },
    ResolveStreaming {
        request_id: Id,
        target: Box<ResolveTarget>,
        profile: String,
        retried_ad_token: bool,
        retried_oauth: bool,
    },
    SubtitleLocate {
        media: Id,
        idx: usize,
        sub: CachedSubtitle,
    },
    SubtitleOpenApiMeta {
        media: Id,
        idx: usize,
        sub: CachedSubtitle,
        api_kind: OpenApiLinkKind,
    },
    SubtitleOpenApiHead {
        media: Id,
        idx: usize,
        sub: CachedSubtitle,
        fallback_dlink: String,
    },
}

pub(crate) struct ActiveHttp {
    pub op: Id,
    pub stream: Option<Id>,
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub request_id: Option<Id>,
    pub flow: HttpFlow,
}

impl ActiveHttp {
    fn cancel(mut self, host: &mut impl Host) {
        host.close(self.op);
        if let Some(s) = self.stream.take() {
            host.close(s);
        }
    }
}

#[derive(Default)]
pub(crate) struct WindowState {
    pub generation: Generation,
    pub qr: Option<PendingQr>,
    pub http_jobs: BTreeMap<Id, ActiveHttp>,
    pub folder_caches: BTreeMap<String, FolderCache>,
    pub browse_seen: BTreeMap<String, BTreeSet<String>>,
    pub browse_cursors: BTreeMap<String, BTreeSet<String>>,
    pub secret_write_op: Option<(Id, StoredSession, bool, bool)>,
    pub active_media: Option<Id>,
    pub last_resolved_video: Option<(String, String)>,
    pub pending_subtitles: Vec<(Id, usize, CachedSubtitle)>,
}

impl WindowState {
    fn clear_scoped(&mut self, host: &mut impl Host) {
        for (_, job) in std::mem::take(&mut self.http_jobs) {
            job.cancel(host);
        }
        self.pending_subtitles.clear();
        self.secret_write_op = None;
    }
}

#[derive(Default)]
pub struct BaiduPanPlugin {
    pub(crate) session: StoredSession,
    pub(crate) quota: Option<QuotaInfo>,
    pub(crate) prefs: Preferences,
    pub(crate) video_lookup: BTreeMap<String, (String, String)>,
    synced_sources_auth: Option<bool>,
    init_secret_op: Option<Id>,
    init_prefs_op: Option<Id>,
    pub(crate) windows: BTreeMap<Id, WindowState>,
}

impl BaiduPanPlugin {
    pub fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        match event {
            HostEvent::Init { .. } => {
                self.init_secret_op = host
                    .submit(&Task::SecretRead {
                        bucket: CredentialBucket::Plugin,
                        key: SECRET_KEY.into(),
                    })
                    .ok();
                self.init_prefs_op = host
                    .submit(&Task::StateRead {
                        key: "prefs".into(),
                    })
                    .ok();
                Ok(Vec::new())
            }
            HostEvent::Completed {
                operation_id,
                result,
                ..
            } => {
                if self.init_secret_op == Some(operation_id) {
                    self.init_secret_op = None;
                    if let Ok(read) = completed::<StoreRead>(result)
                        && let Some(raw) = read.store_value
                        && let Ok(saved) = serde_json::from_str::<StoredSession>(&raw)
                    {
                        self.session = saved;
                    }
                    return Ok(self.maybe_sync_sources().into_iter().collect());
                } else if self.init_prefs_op == Some(operation_id) {
                    self.init_prefs_op = None;
                    if let Ok(read) = completed::<StoreRead>(result)
                        && let Some(raw) = read.store_value
                        && let Ok(saved) = serde_json::from_str::<Preferences>(&raw)
                    {
                        self.prefs = saved;
                    }
                }
                Ok(Vec::new())
            }
            HostEvent::Window {
                window,
                generation,
                body,
            } => {
                if self.windows.len() >= 4 && !self.windows.contains_key(&window) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                let mut state = self.windows.remove(&window).unwrap_or_default();
                if state.generation != generation {
                    state.clear_scoped(host);
                    state.generation = generation;
                }
                let commands = self.handle_window(&mut state, *body, host, now_ms)?;
                self.windows.insert(window, state);
                Ok(commands)
            }
            HostEvent::WindowClosed { window } => {
                self.windows.remove(&window);
                Ok(Vec::new())
            }
            HostEvent::Shutdown => {
                *self = Self::default();
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    pub(crate) fn maybe_sync_sources(&mut self) -> Option<HostCommand> {
        let auth = self.session.is_authenticated();
        if self.synced_sources_auth == Some(auth) {
            return None;
        }
        self.synced_sources_auth = Some(auth);
        Some(ui::sync_sources(auth))
    }

    pub(crate) fn sync_ui(&self, state: &WindowState) -> Vec<HostCommand> {
        ui::sync_window_ui(
            &self.session,
            self.quota.as_ref(),
            &self.prefs,
            state.qr.as_ref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_http(
        &self,
        state: &mut WindowState,
        method: &str,
        url: String,
        headers: Vec<(String, String)>,
        follow_redirects: bool,
        timeout_ms: Option<u32>,
        request_id: Option<Id>,
        flow: HttpFlow,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        if state.http_jobs.len() >= 6 {
            return Err(ErrorCode::Backpressure);
        }
        let op = host.submit(&Task::HttpStart {
            method: method.into(),
            url: url.clone(),
            headers,
            body_handle: None,
            stream_body: false,
            follow_redirects,
            timeout_ms,
            origin_grant: None,
        })?;
        state.http_jobs.insert(
            op,
            ActiveHttp {
                op,
                stream: None,
                status: 0,
                final_url: url,
                headers: Vec::new(),
                body: Vec::new(),
                request_id,
                flow,
            },
        );
        Ok(())
    }

    fn handle_window(
        &mut self,
        state: &mut WindowState,
        event: HostEvent<Value>,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let now_secs = (now_ms / 1000).max(1_700_000_000);
        match event {
            HostEvent::WindowOpened { .. } => {
                if self.session.is_authenticated() {
                    if self.session.uid == 0 || self.session.username.is_none() {
                        let _ = self.request_user_info(state, None, false, false, host);
                    } else if self.quota.is_none() {
                        self.request_quota(state, host);
                    }
                }
                let mut cmds = self.sync_ui(state);
                cmds.extend(self.maybe_sync_sources());
                Ok(cmds)
            }
            HostEvent::RequestCancelled { request_id } => {
                let ids: Vec<Id> = state
                    .http_jobs
                    .iter()
                    .filter_map(|(op, job)| (job.request_id == Some(request_id)).then_some(*op))
                    .collect();
                for op in ids {
                    if let Some(job) = state.http_jobs.remove(&op) {
                        job.cancel(host);
                    }
                }
                Ok(Vec::new())
            }
            HostEvent::Completed {
                operation_id,
                result,
                ..
            } => self.handle_completed(state, operation_id, result, host, now_ms),
            HostEvent::SourceBrowse {
                request_id,
                source_id,
                folder_id,
                page,
                query,
                filters,
            } => Ok(self.begin_browse(
                state, request_id, source_id, folder_id, page, query, filters, false, host, now_ms,
            )),
            HostEvent::SourceResolve {
                request_id,
                source_id,
                entry_id,
                variant,
                ..
            } => Ok(self.begin_resolve(
                state, request_id, &source_id, &entry_id, variant, false, host, now_secs,
            )),
            HostEvent::MediaOpened { snapshot, .. } => {
                state.active_media = snapshot.media;
                state.pending_subtitles.clear();
                if self.prefs.auto_subtitles
                    && let Some(media) = state.active_media
                    && let Some((path, name)) =
                        self.lookup_playing_video(state, snapshot.uri.as_deref())
                    && let Some(parent) = media::parent_dir(&path)
                    && let Some(folder) = state.folder_caches.get(&parent)
                {
                    let mut matched: Vec<CachedSubtitle> = folder
                        .subtitles
                        .iter()
                        .filter(|s| media::subtitle_matches_video(&name, &s.name))
                        .take(4)
                        .cloned()
                        .collect();
                    matched.reverse();
                    for (rev_i, sub) in matched.into_iter().enumerate() {
                        state.pending_subtitles.push((media, rev_i, sub));
                    }
                    self.pump_next_subtitle(state, host, now_secs);
                }
                Ok(Vec::new())
            }
            HostEvent::Command { id, .. } => self.handle_action(state, &id, None, host, now_ms),
            HostEvent::Action { id, value, .. } => {
                self.handle_action(state, &id, value, host, now_ms)
            }
            HostEvent::ModalSubmit { id, values } => {
                self.handle_modal_submit(state, &id, &values, host)
            }
            HostEvent::ModalCancel { id } => {
                if id == "baidupan.login" && state.qr.take().is_some() {
                    Ok(self.sync_ui(state))
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    fn lookup_playing_video(
        &self,
        state: &mut WindowState,
        uri: Option<&str>,
    ) -> Option<(String, String)> {
        let fallback = state.last_resolved_video.take();
        if let Some(u) = uri
            && let Some(rest) = u.strip_prefix("iris-plugin://community.baidupan/")
            && let Some((_, entry_id)) = rest.split_once('/')
        {
            if let Some(found) = self.video_lookup.get(entry_id) {
                return Some(found.clone());
            }
            if let Some((_, path)) = catalog::decode_entry_id(entry_id) {
                let name = media::basename(&path).to_owned();
                return Some((path, name));
            }
        }
        fallback
    }

    fn handle_action(
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
                    Err(e) => cmds.push(ui::toast(format!("获取二维码失败：{e:?}"))),
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
                            cmds.push(ui::toast(format!("检查扫码状态失败：{e:?}")));
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
                    cmds.push(ui::toast(format!("刷新账号信息失败：{e:?}")));
                } else {
                    cmds.push(ui::toast("正在刷新百度网盘账号与空间配额…"));
                }
            }
            "logout" => {
                self.session = StoredSession::default();
                self.quota = None;
                state.qr = None;
                state.folder_caches.clear();
                let _ = host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: SECRET_KEY.into(),
                    value: None,
                });
                cmds.extend(self.sync_ui(state));
                cmds.extend(self.maybe_sync_sources());
                cmds.extend(ui::refresh_all_sources());
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

    fn handle_modal_submit(
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
                Err(e) => Ok(vec![ui::toast(format!("刷新 OAuth 令牌失败：{e:?}"))]),
            };
        }

        match self.request_user_info(state, Some(candidate), true, true, host) {
            Ok(()) => Ok(vec![ui::toast("正在校验百度网盘凭据…")]),
            Err(e) => Ok(vec![ui::toast(format!("无法校验凭据：{e:?}"))]),
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use iris_plugin_sdk::{
        api::v2::{GuestError, Reply},
        guest,
    };
    std::thread_local! {
        static PLUGIN: std::cell::RefCell<BaiduPanPlugin> = std::cell::RefCell::new(BaiduPanPlugin::default());
    }
    fn dispatch(value: Value) -> Reply<HostCommand> {
        let now_ms = guest::clock(false).map_or(0, |id| id.0);
        let result = serde_json::from_value(value)
            .map_err(|_| ErrorCode::InvalidArgument)
            .and_then(|event| {
                PLUGIN.with_borrow_mut(|plugin| {
                    plugin.event(event, &mut iris_plugin_sdk::client::WasmHost, now_ms)
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
