//! Baidu Netdisk (`community.baidupan`) Wasm sandbox plugin for Iris (Protocol 2 / ABI 2),
//! referencing `komorebiCarry/BaiduPCS-Rust` (QR login, Cookie BDUSS/STOKEN, PCS Locate v4)
//! and `OpenListTeam/OpenList` (Open Platform OAuth, `filemetas` / DLNA `dlink` / `streaming`).

mod account_lifecycle;
mod actions;
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
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Default)]
pub(crate) struct VideoCache {
    pub(crate) entries: VecDeque<(String, (String, String))>,
}

impl VideoCache {
    pub(crate) fn insert(&mut self, key: String, val: (String, String)) {
        if key.is_empty() {
            return;
        }
        self.entries.retain(|(k, _)| k != &key);
        while self.entries.len() >= 128 {
            self.entries.pop_front();
        }
        self.entries.push_back((key, val));
    }

    pub(crate) fn get(&self, key: &str) -> Option<&(String, String)> {
        self.entries
            .iter()
            .rev()
            .find_map(|(k, v)| (k == key).then_some(v))
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

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
        account_epoch: u64,
        baiduid: String,
        open_modal: bool,
    },
    UserInfo {
        account_epoch: u64,
        pending_session: Option<Box<StoredSession>>,
        close_modal: bool,
        notify_on_success: bool,
    },
    Quota {
        account_epoch: u64,
    },
    OAuthRefresh {
        account_epoch: u64,
        base_session: Box<StoredSession>,
        retry: Box<OAuthRetry>,
    },
    Browse {
        account_epoch: u64,
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
        original_variant: Option<String>,
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
        original_variant: Option<String>,
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
    pub account_reset: bool,
    pub qr: Option<PendingQr>,
    pub http_jobs: BTreeMap<Id, ActiveHttp>,
    pub folder_caches: BTreeMap<String, FolderCache>,
    pub browse_seen: BTreeMap<String, BTreeSet<String>>,
    pub browse_cursors: BTreeMap<String, BTreeSet<String>>,
    pub browse_page: Option<catalog::PageCache>,
    pub secret_write_op: Option<(Id, u64, StoredSession, Option<QuotaInfo>, bool, bool)>,
    pub logout_secret_op: Option<(Id, u64)>,
    pub background_secret_ops: BTreeSet<Id>,
    pub background_secret_epoch: Option<u64>,
    pub interrupted_secret: Option<account_lifecycle::InterruptedSecret>,
    pub active_media: Option<Id>,
    pub last_resolved_video: Option<(String, String)>,
    pub pending_subtitles: Vec<(Id, usize, CachedSubtitle)>,
}

#[derive(Default)]
pub struct BaiduPanPlugin {
    pub(crate) account_epoch: u64,
    pub(crate) session: StoredSession,
    pub(crate) quota: Option<QuotaInfo>,
    pub(crate) prefs: Preferences,
    pub(crate) video_lookup: VideoCache,
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
                    // The host has already revoked old-generation handles; clear local
                    // scoped tracking without calling host.close on stale IDs.
                    state.discard_scoped();
                    if state.generation.context != generation.context {
                        state.browse_page = None;
                    }
                    state.generation = generation;
                }
                let mut commands = self.flush_account_reset(&mut state, Some(&body), host);
                commands.extend(self.handle_window(&mut state, *body, host, now_ms)?);
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
        let now_secs = now_ms / 1000;
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
                let mut cmds = Vec::new();
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
                    cmds = self.pump_next_subtitle(state, host, now_secs);
                }
                Ok(cmds)
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
        let Some(u) = uri else {
            return fallback;
        };
        let (_source_id, entry_id) = sign::parse_plugin_uri(u)?;
        if let Some(found) = self.video_lookup.get(&entry_id) {
            return Some(found.clone());
        }
        if let Some((_, path)) = catalog::decode_entry_id(&entry_id) {
            let name = media::basename(&path).to_owned();
            return Some((path, name));
        }
        None
    }
}

iris_plugin_sdk::export_typed_plugin!(BaiduPanPlugin, |plugin, event| {
    let now_ms = iris_plugin_sdk::guest::clock(false).map_or(0, |id| id.0);
    plugin.event(event, &mut iris_plugin_sdk::client::WasmHost, now_ms)
});

#[cfg(test)]
mod tests;
