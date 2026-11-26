//! Bilibili (`community.bilibili`) Wasm sandbox plugin for Iris (Protocol 2 / ABI 2),
//! referencing `CKopoer/BiliSail`'s Web/WBI/Passport/PGC/Live/Danmaku APIs.

mod account_lifecycle;
mod actions;
pub mod auth;
pub mod catalog;
pub mod danmaku;
mod enhance;
mod flows;
pub mod playback;
pub mod qr;
pub mod ui;
pub mod wbi;

use auth::{PendingQr, Preferences, StoredSession, WbiKeys};
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
    error_message,
    serde_json::{self, Value},
};
use playback::VideoMeta;
use std::collections::{BTreeMap, BTreeSet};
use wbi::BiliTarget;

pub(crate) struct PendingWbiBrowse {
    pub request_id: Id,
    pub source_id: String,
    pub folder_id: Option<String>,
    pub cursor: Option<String>,
    pub query: Option<String>,
    pub filters: BTreeMap<String, String>,
}

pub(crate) struct PendingWbiPlayurl {
    pub request_id: Id,
    pub requested_entry_id: String,
    pub meta: VideoMeta,
    pub variant: Option<String>,
}

pub(crate) enum HttpFlow {
    Nav {
        account_epoch: u64,
        open_modal: bool,
    },
    QrGenerate {
        open_modal: bool,
    },
    QrPoll {
        account_epoch: u64,
        open_modal: bool,
        manual: bool,
    },
    Browse {
        account_epoch: u64,
        request_id: Id,
        source_id: String,
        kind: BrowseKind,
        retried_wbi: bool,
        used_mixin: Option<String>,
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
        retried_wbi: bool,
        used_mixin: Option<String>,
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
        used_mixin: Option<String>,
    },
    SubtitleBcc {
        media: Id,
        sub: SubtitleTrackMeta,
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
    pub(crate) fn cancel(mut self, host: &mut impl Host) {
        host.close(self.op);
        if let Some(s) = self.stream.take() {
            host.close(s);
        }
    }
}

pub(crate) struct PendingSubtitleArtifact {
    pub op: Id,
    pub writer: Option<Id>,
    pub media: Id,
    pub sub: SubtitleTrackMeta,
    pub vtt_bytes: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct WindowState {
    pub generation: Generation,
    pub account_reset: bool,
    pub qr: Option<PendingQr>,
    pub http_jobs: BTreeMap<Id, ActiveHttp>,
    pub sub_jobs: BTreeMap<Id, PendingSubtitleArtifact>,
    pub pending_subtitles: Vec<(Id, SubtitleTrackMeta)>,
    pub browse_seen: BTreeMap<String, BTreeSet<String>>,
    pub browse_cursors: BTreeMap<String, BTreeSet<String>>,
    pub browse_page: Option<catalog::PageCache>,
    pub pending_wbi_browse: Vec<PendingWbiBrowse>,
    pub pending_wbi_playurl: Vec<PendingWbiPlayurl>,
    pub secret_write_op: Option<(Id, u64, StoredSession, bool)>,
    pub logout_secret_op: Option<(Id, u64)>,
    pub background_secret_ops: BTreeSet<Id>,
    pub background_secret_epoch: Option<u64>,
    pub interrupted_secret: Option<account_lifecycle::InterruptedSecret>,
    pub active_media: Option<Id>,
    pub overlay_gen: Generation,
    pub overlay_window: Option<OverlayWindow>,
    pub active_cid: Option<u64>,
    pub active_bvid: String,
    pub active_aid: u64,
    pub last_resolved_cid: Option<(String, u64, u64)>,
    pub needs_wbi_player_v2: bool,
    pub danmaku: Vec<DanmakuItem>,
}

#[derive(Default)]
pub struct BilibiliPlugin {
    pub(crate) account_epoch: u64,
    pub(crate) session: StoredSession,
    pub(crate) prefs: Preferences,
    pub(crate) wbi: WbiKeys,
    pub(crate) cid_lookup: enhance::CidCache,
    synced_sources_auth: Option<bool>,
    pub(crate) init_secret_op: Option<Id>,
    init_prefs_op: Option<Id>,
    pub(crate) windows: BTreeMap<Id, WindowState>,
}

impl BilibiliPlugin {
    pub fn event(
        &mut self,
        event: HostEvent<Value>,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let now_secs = now_ms / 1000;
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
                    // The host has already revoked old-generation handles; clear local
                    // scoped tracking without calling host.close on stale IDs.
                    state.discard_scoped();
                    if state.generation.context != generation.context {
                        state.browse_page = None;
                    }
                    if state.generation.media != generation.media {
                        state.active_cid = None;
                        state.danmaku.clear();
                        state.overlay_window = None;
                    }
                    state.generation = generation;
                }
                let mut commands = self.flush_account_reset(&mut state, Some(&body), host);
                commands.extend(self.handle_window(window, &mut state, *body, host, now_secs)?);
                self.resume_interrupted_secret(&mut state, host);
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

    pub(crate) fn sync_ui(&self, state: &WindowState) -> Vec<HostCommand> {
        ui::sync_window_ui(
            &self.session,
            &self.prefs,
            state.qr.as_ref(),
            state.active_cid,
            state.danmaku.len(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_http(
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

    pub(crate) fn ensure_nav_in_flight(
        &self,
        state: &mut WindowState,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        if state
            .http_jobs
            .values()
            .any(|j| matches!(j.flow, HttpFlow::Nav { .. }))
        {
            return Ok(());
        }
        self.start_http(
            state,
            "GET",
            "https://api.bilibili.com/x/web-interface/nav".into(),
            true,
            None,
            HttpFlow::Nav {
                account_epoch: self.account_epoch,
                open_modal: false,
            },
            host,
        )
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
                if !self.wbi.is_fresh(now_secs)
                    || (self.session.is_authenticated() && self.session.dede_user_id.is_empty())
                {
                    let _ = self.ensure_nav_in_flight(state, host);
                }
                let mut cmds = self.sync_ui(state);
                cmds.extend(self.maybe_sync_sources());
                Ok(cmds)
            }
            HostEvent::RequestCancelled { request_id } => {
                state
                    .pending_wbi_browse
                    .retain(|p| p.request_id != request_id);
                state
                    .pending_wbi_playurl
                    .retain(|p| p.request_id != request_id);
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
                        cmds.extend(
                            self.fetch_enhancements(state, media, &bvid, aid, cid, host, now_secs),
                        );
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

    pub(crate) fn begin_resolve_target(
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
                message: format!("无法提交解析任务：{}", error_message(e)),
            }]
        } else {
            Vec::new()
        }
    }
}

iris_plugin_sdk::export_typed_plugin!(BilibiliPlugin, |plugin, event| {
    let now_ms = iris_plugin_sdk::guest::clock(false).map_or(0, |id| id.0);
    plugin.event(event, &mut iris_plugin_sdk::client::WasmHost, now_ms)
});

#[cfg(test)]
mod tests;
