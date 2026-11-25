//! Bilibili (`community.bilibili`) Wasm sandbox plugin for Iris (Protocol 2 / ABI 2),
//! referencing `CKopoer/BiliSail`'s Web/WBI/Passport/PGC/Live/Danmaku APIs.

pub mod auth;
pub mod catalog;
pub mod danmaku;
mod enhance;
mod flows;
pub mod playback;
pub mod qr;
pub mod ui;
pub mod wbi;

use auth::{PendingQr, Preferences, QrPollOutcome, StoredSession, WbiKeys};
use catalog::BrowseKind;
use danmaku::{DanmakuItem, SubtitleTrackMeta};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, Generation, HostCommand, HostEvent, Id,
        enhance::OverlayWindow,
        task::{CredentialBucket, Task},
        task_result::StoreRead,
    },
    client::{Host, completed},
    serde_json::{self, Value},
};
use playback::VideoMeta;
use std::collections::{BTreeMap, BTreeSet};
use wbi::BiliTarget;

enum HttpFlow {
    Nav {
        open_modal: bool,
    },
    QrGenerate {
        open_modal: bool,
    },
    QrPoll {
        open_modal: bool,
        manual: bool,
    },
    Browse {
        request_id: Id,
        source_id: String,
        kind: BrowseKind,
        retried_wbi: bool,
        query: Option<String>,
        folder_id: Option<String>,
        cursor: Option<String>,
        filters: BTreeMap<String, String>,
    },
    ShortRedirect {
        request_id: Id,
        source_id: String,
        variant: Option<String>,
    },
    ResolveView {
        request_id: Id,
        source_id: String,
        requested_entry_id: String,
        cid: Option<u64>,
        page: usize,
        variant: Option<String>,
    },
    ResolveSeason {
        request_id: Id,
        source_id: String,
        requested_entry_id: String,
        target_ep: Option<u64>,
        variant: Option<String>,
    },
    ResolveVodPlay {
        request_id: Id,
        requested_entry_id: String,
        meta: Box<VideoMeta>,
        variant: Option<String>,
    },
    ResolveLivePlay {
        request_id: Id,
        room_id: u64,
        source_id: String,
        variant: Option<String>,
    },
    DanmakuXml {
        media: Id,
        cid: u64,
    },
    PlayerV2 {
        media: Id,
        cid: u64,
        retried_wbi: bool,
    },
    SubtitleBcc {
        media: Id,
        sub: SubtitleTrackMeta,
    },
}

struct ActiveHttp {
    op: Id,
    stream: Option<Id>,
    status: u16,
    final_url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    request_id: Option<Id>,
    flow: HttpFlow,
}

impl ActiveHttp {
    fn cancel(mut self, host: &mut impl Host) {
        host.close(self.op);
        if let Some(s) = self.stream.take() {
            host.close(s);
        }
    }
}

struct PendingSubtitleArtifact {
    op: Id,
    writer: Option<Id>,
    media: Id,
    sub: SubtitleTrackMeta,
    vtt_bytes: Vec<u8>,
}

#[derive(Default)]
struct WindowState {
    generation: Generation,
    qr: Option<PendingQr>,
    http_jobs: BTreeMap<Id, ActiveHttp>,
    sub_jobs: BTreeMap<Id, PendingSubtitleArtifact>,
    pending_subtitles: Vec<(Id, SubtitleTrackMeta)>,
    browse_seen: BTreeMap<String, BTreeSet<String>>,
    browse_cursors: BTreeMap<String, BTreeSet<String>>,
    secret_write_op: Option<(Id, StoredSession, bool)>,
    active_media: Option<Id>,
    overlay_gen: Generation,
    overlay_window: Option<OverlayWindow>,
    active_cid: Option<u64>,
    active_bvid: String,
    active_aid: u64,
    last_resolved_cid: Option<(String, u64, u64)>,
    needs_wbi_player_v2: bool,
    danmaku: Vec<DanmakuItem>,
}

impl WindowState {
    fn clear_scoped(&mut self, host: &mut impl Host) {
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
        self.secret_write_op = None;
        self.needs_wbi_player_v2 = false;
    }
}

#[derive(Default)]
pub struct BilibiliPlugin {
    session: StoredSession,
    prefs: Preferences,
    wbi: WbiKeys,
    cid_lookup: BTreeMap<String, (String, u64, u64)>,
    synced_sources_auth: Option<bool>,
    init_secret_op: Option<Id>,
    init_prefs_op: Option<Id>,
    windows: BTreeMap<Id, WindowState>,
}

impl BilibiliPlugin {
    pub fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let now_secs = (now_ms / 1000).max(1_700_000_000);
        match event {
            HostEvent::Init { .. } => {
                self.ensure_buvid3(host, now_ms);
                self.init_secret_op = host
                    .submit(&Task::SecretRead {
                        bucket: CredentialBucket::Plugin,
                        key: "bili.session".into(),
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
                        && let Ok(mut saved) = serde_json::from_str::<StoredSession>(&raw)
                    {
                        if saved.buvid3.is_empty() {
                            saved.buvid3 = self.session.buvid3.clone();
                        }
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
                    if state.generation.media != generation.media {
                        state.active_cid = None;
                        state.danmaku.clear();
                        state.overlay_window = None;
                    }
                    state.generation = generation;
                }
                let commands = self.handle_window(window, &mut state, *body, host, now_secs)?;
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

    fn ensure_buvid3(&mut self, host: &mut impl Host, now_ms: u64) {
        if !self.session.buvid3.is_empty() {
            return;
        }
        let mut entropy = [0x42u8; 16];
        let _ = host.random(&mut entropy);
        self.session.buvid3 = wbi::generate_buvid3(&entropy, now_ms);
    }

    pub(crate) fn maybe_sync_sources(&mut self) -> Option<HostCommand> {
        let auth = self.session.is_authenticated();
        if self.synced_sources_auth == Some(auth) {
            return None;
        }
        self.synced_sources_auth = Some(auth);
        Some(ui::sync_sources(auth))
    }

    fn sync_ui(&self, state: &WindowState) -> Vec<HostCommand> {
        ui::sync_window_ui(
            &self.session,
            &self.prefs,
            state.qr.as_ref(),
            state.active_cid,
            state.danmaku.len(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_http(
        &self,
        state: &mut WindowState,
        method: &str,
        url: String,
        follow_redirects: bool,
        request_id: Option<Id>,
        flow: HttpFlow,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        if state.http_jobs.len() >= 6 {
            return Err(ErrorCode::Backpressure);
        }
        let headers = if matches!(flow, HttpFlow::SubtitleBcc { .. }) {
            self.session.media_headers(false)
        } else {
            self.session.api_headers()
        };
        let op = host.submit(&Task::HttpStart {
            method: method.into(),
            url: url.clone(),
            headers,
            body_handle: None,
            stream_body: false,
            follow_redirects,
            timeout_ms: Some(25_000),
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
        _window: Id,
        state: &mut WindowState,
        event: HostEvent<Value>,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        match event {
            HostEvent::WindowOpened { .. } => {
                if !self.wbi.is_fresh(now_secs) {
                    let _ = self.start_http(
                        state,
                        "GET",
                        "https://api.bilibili.com/x/web-interface/nav".into(),
                        true,
                        None,
                        HttpFlow::Nav { open_modal: false },
                        host,
                    );
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
            } => self.handle_completed(state, operation_id, result, host, now_secs),
            HostEvent::SourceBrowse {
                request_id,
                source_id,
                folder_id,
                page,
                query,
                filters,
            } => Ok(self.begin_browse(
                state, request_id, source_id, folder_id, page, query, filters, false, host,
                now_secs,
            )),
            HostEvent::SourceResolve {
                request_id,
                source_id,
                entry_id,
                variant,
                ..
            } => {
                Ok(self
                    .begin_resolve_target(state, request_id, &source_id, &entry_id, variant, host))
            }
            HostEvent::ResolveUrl { request_id, url } => {
                Ok(self.begin_resolve_target(state, request_id, "recommend", &url, None, host))
            }
            HostEvent::MediaOpened { snapshot, .. } => {
                state.active_media = snapshot.media;
                state.overlay_gen = state.generation;
                state.danmaku.clear();
                state.active_cid = None;
                state.needs_wbi_player_v2 = false;
                let mut cmds = Vec::new();
                let resolved = snapshot
                    .uri
                    .as_deref()
                    .and_then(|uri| self.lookup_playing_cid(uri))
                    .or_else(|| {
                        snapshot
                            .uri
                            .is_none()
                            .then(|| state.last_resolved_cid.take())
                            .flatten()
                    });
                state.last_resolved_cid = None;
                if let Some((bvid, aid, cid)) = resolved {
                    state.active_cid = Some(cid);
                    state.active_bvid = bvid.clone();
                    state.active_aid = aid;
                    if let Some(media) = state.active_media {
                        self.fetch_enhancements(state, media, &bvid, aid, cid, host, now_secs);
                    }
                    cmds.extend(self.sync_ui(state));
                }
                Ok(cmds)
            }
            HostEvent::OverlayWindowChanged {
                media,
                generation,
                window,
            } => {
                state.active_media = Some(media);
                state.overlay_gen = generation;
                state.overlay_window = Some(window);
                Ok(self.push_danmaku_window(state, host))
            }
            HostEvent::Command { id, .. } => self.handle_action(state, &id, None, host, now_secs),
            HostEvent::Action { id, value, .. } => {
                self.handle_action(state, &id, value, host, now_secs)
            }
            HostEvent::ModalSubmit { id, values } => {
                self.handle_modal_submit(state, &id, &values, host)
            }
            HostEvent::ModalCancel { id } => {
                if id == "bili.login" && state.qr.take().is_some() {
                    Ok(self.sync_ui(state))
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    fn begin_resolve_target(
        &self,
        state: &mut WindowState,
        request_id: Id,
        source_id: &str,
        raw_target: &str,
        variant: Option<String>,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        let Some(target) = wbi::parse_target(raw_target) else {
            return vec![HostCommand::SourceError {
                request_id,
                message: "无法识别该哔哩哔哩视频/番剧/直播地址".into(),
            }];
        };
        let requested_entry_id = raw_target.to_owned();
        let res = match target {
            BiliTarget::Bvid { bvid, cid, page } => self.start_http(
                state,
                "GET",
                format!(
                    "https://api.bilibili.com/x/web-interface/view?bvid={}",
                    wbi::encode_query_component(&bvid)
                ),
                true,
                Some(request_id),
                HttpFlow::ResolveView {
                    request_id,
                    source_id: source_id.into(),
                    requested_entry_id,
                    cid,
                    page,
                    variant,
                },
                host,
            ),
            BiliTarget::Aid { aid, cid, page } => self.start_http(
                state,
                "GET",
                format!("https://api.bilibili.com/x/web-interface/view?aid={aid}"),
                true,
                Some(request_id),
                HttpFlow::ResolveView {
                    request_id,
                    source_id: source_id.into(),
                    requested_entry_id,
                    cid,
                    page,
                    variant,
                },
                host,
            ),
            BiliTarget::Episode(ep_id) => self.start_http(
                state,
                "GET",
                format!("https://api.bilibili.com/pgc/view/web/season?ep_id={ep_id}"),
                true,
                Some(request_id),
                HttpFlow::ResolveSeason {
                    request_id,
                    source_id: source_id.into(),
                    requested_entry_id,
                    target_ep: Some(ep_id),
                    variant,
                },
                host,
            ),
            BiliTarget::Season(season_id) => self.start_http(
                state,
                "GET",
                format!("https://api.bilibili.com/pgc/view/web/season?season_id={season_id}"),
                true,
                Some(request_id),
                HttpFlow::ResolveSeason {
                    request_id,
                    source_id: source_id.into(),
                    requested_entry_id,
                    target_ep: None,
                    variant,
                },
                host,
            ),
            BiliTarget::LiveRoom(room_id) => self.start_http(
                state,
                "GET",
                format!(
                    "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo?room_id={room_id}&protocol=0,1&format=0,1,2&codec=0,1&qn=10000&platform=web"
                ),
                true,
                Some(request_id),
                HttpFlow::ResolveLivePlay {
                    request_id,
                    room_id,
                    source_id: source_id.into(),
                    variant,
                },
                host,
            ),
            BiliTarget::ShortUrl(short) => self.start_http(
                state,
                "GET",
                short,
                false,
                Some(request_id),
                HttpFlow::ShortRedirect {
                    request_id,
                    source_id: source_id.into(),
                    variant,
                },
                host,
            ),
        };
        if let Err(e) = res {
            vec![HostCommand::SourceError {
                request_id,
                message: format!("无法提交解析任务：{e:?}"),
            }]
        } else {
            Vec::new()
        }
    }

    fn handle_action(
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
                    Err(e) => cmds.extend(self.fail_http_flow(&flow, e)),
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
                        if let Err(e) = self.start_http(
                            state,
                            "GET",
                            url,
                            false,
                            None,
                            HttpFlow::QrPoll { open_modal, manual },
                            host,
                        ) && manual
                        {
                            cmds.extend(
                                self.fail_http_flow(&HttpFlow::QrPoll { open_modal, manual }, e),
                            );
                        }
                    }
                } else if manual {
                    cmds.push(ui::toast("请先生成登录二维码"));
                }
            }
            "logout" => {
                let buvid3 = self.session.buvid3.clone();
                self.session = StoredSession {
                    buvid3,
                    ..Default::default()
                };
                state.qr = None;
                let _ = host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: "bili.session".into(),
                    value: None,
                });
                cmds.extend(self.sync_ui(state));
                cmds.extend(self.maybe_sync_sources());
                cmds.extend(ui::refresh_all_sources());
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
                    self.fetch_enhancements(state, media, &bvid, aid, cid, host, now_secs);
                    cmds.push(ui::toast("正在刷新当前视频弹幕与字幕…"));
                } else {
                    cmds.push(ui::toast("当前未在播放哔哩哔哩点播视频"));
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
                        state.secret_write_op = Some((op, parsed, true));
                        Ok(Vec::new())
                    }
                    Err(e) => Ok(vec![ui::toast(format!("保存凭据失败：{e:?}"))]),
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

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use iris_plugin_sdk::{
        api::v2::{GuestError, Reply},
        guest,
    };
    std::thread_local! {
        static PLUGIN: std::cell::RefCell<BilibiliPlugin> = std::cell::RefCell::new(BilibiliPlugin::default());
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
