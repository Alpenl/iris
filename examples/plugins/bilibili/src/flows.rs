//! Async task completions, HTTP body buffering, WebVTT artifact staging, and danmaku windows.

mod account;
mod resolve;

use crate::{
    ActiveHttp, BilibiliPlugin, HttpFlow, PendingWbiBrowse, WindowState, auth::WbiKeys, catalog,
    danmaku, ui,
};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, HostCommand, Id, LogLevel,
        task::Task,
        task_result::{HttpResult, ReadResult, StoreWrite},
    },
    client::{Host, completed},
    error_message,
    serde_json::{self, Value},
};
use std::collections::BTreeMap;

const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;

impl BilibiliPlugin {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_browse(
        &self,
        state: &mut WindowState,
        request_id: Id,
        source_id: String,
        folder_id: Option<String>,
        cursor: Option<String>,
        query: Option<String>,
        filters: BTreeMap<String, String>,
        retried_wbi: bool,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        if let Some(cursor) = cursor.as_deref().filter(|c| c.starts_with("local:")) {
            let target = catalog::PageTarget {
                account_epoch: self.account_epoch,
                source: source_id,
                folder: folder_id,
                query,
                filters,
            };
            let result = state
                .browse_page
                .as_mut()
                .filter(|cache| {
                    (!cache.requires_auth() || self.session.is_authenticated())
                        && cache.matches(&target, cursor)
                })
                .ok_or_else(|| "目录分页已失效，请刷新目录".to_owned())
                .and_then(catalog::PageCache::next);
            if state
                .browse_page
                .as_ref()
                .is_some_and(|cache| !cache.has_more())
            {
                state.browse_page = None;
            }
            return match result {
                Ok(page) => vec![HostCommand::ProvideSourcePage {
                    request_id,
                    page: Box::new(page),
                }],
                Err(message) => vec![HostCommand::SourceError {
                    request_id,
                    message,
                }],
            };
        }
        state.browse_page = None;
        let plan = match catalog::plan_browse(
            &source_id,
            folder_id.as_deref(),
            cursor.as_deref(),
            query.as_deref(),
            &filters,
            &self.session,
            &self.wbi,
            now_secs,
        ) {
            Ok(p) => p,
            Err(message) => {
                let mut cmds = Vec::new();
                if now_secs == 0 && self.wbi.mixin().is_some() {
                    cmds.push(HostCommand::Log {
                        level: LogLevel::Error,
                        message: "bilibili: system wall clock unavailable (now_ms == 0); refusing to WBI-sign browse request".into(),
                    });
                }
                cmds.push(HostCommand::SourceError {
                    request_id,
                    message,
                });
                return cmds;
            }
        };
        if plan.needs_wbi || (plan.needs_mid && (!retried_wbi || source_id == "favorites")) {
            if plan.needs_wbi && now_secs == 0 {
                return vec![
                    HostCommand::Log {
                        level: LogLevel::Error,
                        message: "bilibili: system wall clock unavailable (now_ms == 0); refusing to WBI-sign browse request".into(),
                    },
                    HostCommand::SourceError {
                        request_id,
                        message: "系统时钟不可用，无法生成 B站 WBI 签名".into(),
                    },
                ];
            }
            if retried_wbi {
                let message = if plan.needs_mid {
                    "未能获取哔哩哔哩账号 UID，请检查 SESSDATA 是否有效或重新登录"
                } else {
                    "获取 B站 WBI 签名密钥失败，请稍后重试"
                };
                return vec![HostCommand::SourceError {
                    request_id,
                    message: message.into(),
                }];
            }
            if let Err(e) = self.ensure_nav_in_flight(state, host) {
                return vec![HostCommand::SourceError {
                    request_id,
                    message: format!("刷新 B站账号/WBI 状态失败：{}", error_message(e)),
                }];
            }
            if state.pending_wbi_browse.len() < 8 {
                state.pending_wbi_browse.push(PendingWbiBrowse {
                    request_id,
                    source_id,
                    folder_id,
                    cursor,
                    query,
                    filters,
                });
                return Vec::new();
            }
            return vec![HostCommand::SourceError {
                request_id,
                message: "待重试的 B站 目录请求过多，请稍后重试".into(),
            }];
        }
        let used_mixin = self.wbi.mixin();
        if let Err(e) = self.start_http(
            state,
            "GET",
            plan.url,
            true,
            Some(request_id),
            HttpFlow::Browse {
                account_epoch: self.account_epoch,
                request_id,
                source_id,
                kind: plan.kind,
                retried_wbi,
                used_mixin,
                query,
                folder_id,
                cursor,
                filters,
            },
            host,
        ) {
            return vec![HostCommand::SourceError {
                request_id,
                message: format!("无法发起目录请求：{}", error_message(e)),
            }];
        }
        Vec::new()
    }

    pub(crate) fn persist_prefs(&self, host: &mut impl Host) {
        if let Ok(raw) = serde_json::to_string(&self.prefs) {
            let _ = host.submit(&Task::StateWrite {
                key: "prefs".into(),
                value: Some(raw),
            });
        }
    }

    pub(super) fn handle_completed(
        &mut self,
        state: &mut WindowState,
        op: Id,
        result: Value,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        if state.logout_secret_op.is_some_and(|(id, _)| id == op) {
            state.logout_secret_op = None;
            return Ok(Vec::new());
        }
        if state.background_secret_ops.remove(&op) {
            if state.background_secret_ops.is_empty() {
                state.background_secret_epoch = None;
            }
            return Ok(Vec::new());
        }
        if state
            .secret_write_op
            .as_ref()
            .is_some_and(|(id, ..)| *id == op)
        {
            let (_, account_epoch, next_session, close_modal) =
                state.secret_write_op.take().unwrap();
            if account_epoch != self.account_epoch {
                return Ok(Vec::new());
            }
            if completed::<StoreWrite>(result).is_ok_and(|w| w.stored) {
                let mut cmds = self.commit_session(state, next_session, host);
                let nav_pending_mid = self.session.dede_user_id.is_empty()
                    && self.ensure_nav_in_flight(state, host).is_ok();
                if !nav_pending_mid {
                    let _ = self.ensure_nav_in_flight(state, host);
                }
                cmds.extend(self.sync_ui(state));
                cmds.extend(self.maybe_sync_sources());
                if close_modal {
                    cmds.push(HostCommand::CloseModal {
                        id: Some("bili.login".into()),
                    });
                }
                for cmd in ui::refresh_all_sources() {
                    if nav_pending_mid
                        && matches!(&cmd, HostCommand::RefreshSource { source_id } if matches!(source_id.as_str(), "favorites" | "bangumi"))
                    {
                        continue;
                    }
                    cmds.push(cmd);
                }
                cmds.push(ui::toast("哔哩哔哩登录成功"));
                return Ok(cmds);
            }
            return Ok(vec![ui::toast("系统凭据存储未能写入，请稍后重试")]);
        }

        if let Some(sub_job) = state.sub_jobs.remove(&op) {
            return Ok(self.handle_sub_artifact_completed(state, sub_job, result, host));
        }

        let Some(mut job) = state.http_jobs.remove(&op) else {
            return Ok(Vec::new());
        };
        if job.stream.is_none() {
            match completed::<HttpResult>(result) {
                Ok(HttpResult::Response {
                    stream_handle,
                    status,
                    url,
                    headers,
                    ..
                }) => {
                    job.status = status;
                    job.final_url = url;
                    job.headers = headers;
                    if matches!(job.flow, HttpFlow::ShortRedirect { .. }) {
                        host.close(stream_handle);
                        return Ok(self.finish_http(state, job, host, now_secs));
                    }
                    match host.submit(&Task::HttpRead {
                        handle: stream_handle,
                        len: 256 * 1024,
                    }) {
                        Ok(read_op) => {
                            job.op = read_op;
                            job.stream = Some(stream_handle);
                            state.http_jobs.insert(read_op, job);
                            return Ok(Vec::new());
                        }
                        Err(e) => {
                            host.close(stream_handle);
                            if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                                self.pump_next_subtitle(state, host);
                            }
                            return Ok(self.fail_http_flow(state, &job.flow, e));
                        }
                    }
                }
                Ok(_) => {
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return Ok(self.fail_http_flow(state, &job.flow, ErrorCode::BadState));
                }
                Err(e) => {
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return Ok(self.fail_http_flow(state, &job.flow, e));
                }
            }
        }

        match completed::<ReadResult>(result) {
            Ok(read) => {
                let chunk = host.take_blob(read.blob)?;
                if job.body.len() + chunk.len() > MAX_HTTP_BODY {
                    if let Some(s) = job.stream.take() {
                        host.close(s);
                    }
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return Ok(self.fail_http_flow(state, &job.flow, ErrorCode::QuotaExceeded));
                }
                job.body.extend_from_slice(&chunk);
                if read.eof {
                    if let Some(s) = job.stream.take() {
                        host.close(s);
                    }
                    return Ok(self.finish_http(state, job, host, now_secs));
                }
                let handle = job.stream.unwrap();
                match host.submit(&Task::HttpRead {
                    handle,
                    len: 256 * 1024,
                }) {
                    Ok(read_op) => {
                        job.op = read_op;
                        state.http_jobs.insert(read_op, job);
                        Ok(Vec::new())
                    }
                    Err(e) => {
                        host.close(handle);
                        if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                            self.pump_next_subtitle(state, host);
                        }
                        Ok(self.fail_http_flow(state, &job.flow, e))
                    }
                }
            }
            Err(e) => {
                if let Some(s) = job.stream.take() {
                    host.close(s);
                }
                if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                    self.pump_next_subtitle(state, host);
                }
                Ok(self.fail_http_flow(state, &job.flow, e))
            }
        }
    }

    pub(crate) fn fail_http_flow(
        &self,
        state: &mut WindowState,
        flow: &HttpFlow,
        code: ErrorCode,
    ) -> Vec<HostCommand> {
        let denied_hint = "（请确认已授予插件网络权限，且代理未对 *.bilibili.com 返回 198.18.0.0/15 或 2001:2::/48 等 Fake-IP）";
        match flow {
            HttpFlow::Nav { .. } => {
                let message = format!("刷新 B站 WBI 签名密钥失败 ({})", error_message(code));
                std::mem::take(&mut state.pending_wbi_browse)
                    .into_iter()
                    .map(|item| item.request_id)
                    .chain(
                        std::mem::take(&mut state.pending_wbi_playurl)
                            .into_iter()
                            .map(|item| item.request_id),
                    )
                    .map(|request_id| HostCommand::SourceError {
                        request_id,
                        message: message.clone(),
                    })
                    .collect()
            }
            HttpFlow::Browse { request_id, .. }
            | HttpFlow::ShortRedirect { request_id, .. }
            | HttpFlow::ResolveView { request_id, .. }
            | HttpFlow::ResolveSeason { request_id, .. }
            | HttpFlow::ResolveVodPlay { request_id, .. }
            | HttpFlow::ResolveLivePlay { request_id, .. } => vec![HostCommand::SourceError {
                request_id: *request_id,
                message: if code == ErrorCode::Denied {
                    format!(
                        "哔哩哔哩网络请求被拒绝：{}{denied_hint}",
                        error_message(code)
                    )
                } else {
                    format!("哔哩哔哩网络请求失败 ({})", error_message(code))
                },
            }],
            HttpFlow::QrGenerate { .. } | HttpFlow::QrPoll { manual: true, .. } => {
                vec![ui::toast(if code == ErrorCode::Denied {
                    format!("通行证请求失败：{}{denied_hint}", error_message(code))
                } else {
                    format!("通行证请求失败：{}", error_message(code))
                })]
            }
            _ => Vec::new(),
        }
    }

    fn finish_http(
        &mut self,
        state: &mut WindowState,
        job: ActiveHttp,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        if !matches!(job.flow, HttpFlow::ShortRedirect { .. }) {
            let decoded = match danmaku::decode_http_body(&job.headers, &job.body, MAX_HTTP_BODY) {
                Ok(bytes) => bytes,
                Err(_) => {
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return self.fail_http_flow(state, &job.flow, ErrorCode::Unsupported);
                }
            };
            return self.finish_decoded_http(state, job.flow, job.headers, decoded, host, now_secs);
        }
        let HttpFlow::ShortRedirect {
            request_id,
            source_id,
            variant,
        } = job.flow
        else {
            return Vec::new();
        };
        let loc = job
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map(|(_, v)| v.clone())
            .unwrap_or(job.final_url);
        if loc.contains("b23.tv") {
            vec![HostCommand::SourceError {
                request_id,
                message: "短链接未跳转到有效的 bilibili 地址".into(),
            }]
        } else {
            self.begin_resolve_target(state, request_id, &source_id, &loc, variant, host)
        }
    }

    fn finish_decoded_http(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
            HttpFlow::DanmakuXml { media, cid } => {
                if state.active_media != Some(media) || state.active_cid != Some(cid) {
                    return Vec::new();
                }
                let Ok(xml) = std::str::from_utf8(&body) else {
                    return Vec::new();
                };
                state.danmaku = danmaku::parse_danmaku_xml(xml);
                let mut cmds = self.push_danmaku_window(state, host);
                cmds.extend(self.sync_ui(state));
                cmds
            }
            HttpFlow::SubtitleBcc { media, sub } => {
                self.stage_subtitle_bcc(state, media, sub, &body, host);
                Vec::new()
            }
            other => {
                let Ok(json) = serde_json::from_slice::<Value>(&body) else {
                    return self.fail_http_flow(state, &other, ErrorCode::InvalidArgument);
                };
                self.finish_json_flow(state, other, &json, &headers, host, now_secs)
            }
        }
    }

    fn finish_json_flow(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        json: &Value,
        headers: &[(String, String)],
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
            flow @ (HttpFlow::Nav { .. }
            | HttpFlow::QrGenerate { .. }
            | HttpFlow::QrPoll { .. }) => {
                self.finish_account_json_flow(state, flow, json, headers, host, now_secs)
            }
            HttpFlow::Browse {
                account_epoch,
                request_id,
                source_id,
                kind,
                retried_wbi,
                used_mixin,
                query,
                folder_id,
                cursor,
                filters,
            } => {
                if account_epoch != self.account_epoch {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "账号状态已变更，请刷新目录重试".into(),
                    }];
                }
                if json.get("code").and_then(Value::as_i64) == Some(-403) && !retried_wbi {
                    let current_mixin = self.wbi.mixin();
                    if current_mixin.is_some() && current_mixin != used_mixin {
                        return self.begin_browse(
                            state, request_id, source_id, folder_id, cursor, query, filters, true,
                            host, now_secs,
                        );
                    }
                    self.wbi = WbiKeys::default();
                    if let Err(e) = self.ensure_nav_in_flight(state, host) {
                        return vec![HostCommand::SourceError {
                            request_id,
                            message: format!("刷新 B站 WBI 签名密钥失败：{}", error_message(e)),
                        }];
                    }
                    if state.pending_wbi_browse.len() < 8 {
                        state.pending_wbi_browse.push(PendingWbiBrowse {
                            request_id,
                            source_id,
                            folder_id,
                            cursor,
                            query,
                            filters,
                        });
                        return Vec::new();
                    }
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "待重试的 B站 目录请求过多，请稍后重试".into(),
                    }];
                }
                let requires_auth = kind.requires_auth();
                match catalog::parse_browse_response(
                    &source_id,
                    &kind,
                    json,
                    self.session.is_authenticated(),
                ) {
                    Ok(mut page) => {
                        // Authentication and root pages must echo the same view identity.
                        page.folder_id = folder_id.clone();
                        catalog::dedup_browse_page(
                            &mut state.browse_seen,
                            &mut state.browse_cursors,
                            &source_id,
                            folder_id.as_deref(),
                            query.as_deref(),
                            &filters,
                            cursor.as_deref(),
                            &mut page,
                        );
                        let target = catalog::PageTarget {
                            account_epoch: self.account_epoch,
                            source: source_id,
                            folder: folder_id,
                            query,
                            filters,
                        };
                        let mut cache =
                            catalog::PageCache::new(target, request_id, requires_auth, page);
                        match cache.next() {
                            Ok(page) => {
                                state.browse_page = cache.has_more().then_some(cache);
                                vec![HostCommand::ProvideSourcePage {
                                    request_id,
                                    page: Box::new(page),
                                }]
                            }
                            Err(message) => vec![HostCommand::SourceError {
                                request_id,
                                message,
                            }],
                        }
                    }
                    Err(message) => vec![HostCommand::SourceError {
                        request_id,
                        message,
                    }],
                }
            }
            flow @ (HttpFlow::ResolveView { .. }
            | HttpFlow::ResolveSeason { .. }
            | HttpFlow::ResolveVodPlay { .. }
            | HttpFlow::ResolveLivePlay { .. }) => {
                self.finish_resolve_json_flow(state, flow, json, host, now_secs)
            }
            HttpFlow::PlayerV2 {
                media,
                cid,
                retried_wbi,
                used_mixin,
            } => {
                if state.active_media != Some(media) || state.active_cid != Some(cid) {
                    return Vec::new();
                }
                if json.get("code").and_then(Value::as_i64) == Some(-403) && !retried_wbi {
                    let current_mixin = self.wbi.mixin();
                    if current_mixin.is_some() && current_mixin != used_mixin {
                        let (bvid, aid) = (state.active_bvid.clone(), state.active_aid);
                        return self.request_player_v2(
                            state, media, &bvid, aid, cid, true, host, now_secs,
                        );
                    }
                    self.wbi = WbiKeys::default();
                    state.needs_wbi_player_v2 = true;
                    let _ = self.ensure_nav_in_flight(state, host);
                    return Vec::new();
                }
                let (chapters, subtitles) = danmaku::parse_player_v2_info(json);
                if !subtitles.is_empty() {
                    state.needs_wbi_player_v2 = false;
                }
                state.pending_subtitles = subtitles
                    .into_iter()
                    .take(4)
                    .map(|sub| (media, sub))
                    .collect();
                self.pump_next_subtitle(state, host);
                if !chapters.is_empty() {
                    vec![HostCommand::SetTimelineMeta {
                        media,
                        generation: state.generation,
                        chapters,
                        heat: Vec::new(),
                        skip: Vec::new(),
                    }]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}
