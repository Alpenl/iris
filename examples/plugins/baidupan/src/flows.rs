//! Asynchronous HTTP pump, request lifecycle, and browse flow dispatch.
//! Account/QR/OAuth transitions live in `flows/account.rs`, stream resolution
//! in `flows/resolve.rs`, and automatic sibling subtitles in `flows/subtitles.rs`.

mod account;
mod resolve;
mod subtitles;

use crate::{
    ActiveHttp, BaiduPanPlugin, HttpFlow, OAuthRetry, WindowState, auth, catalog, sign, ui,
};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, HostCommand, Id,
        task::Task,
        task_result::{HttpResult, ReadResult, StoreWrite},
    },
    client::{Host, completed},
    error_message,
    serde_json::{self, Value},
};
use std::collections::BTreeMap;

const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;

impl BaiduPanPlugin {
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
        retried_oauth: bool,
        host: &mut impl Host,
        now_ms: u64,
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
                .filter(|cache| self.session.is_authenticated() && cache.matches(&target, cursor))
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
            &self.prefs,
            now_ms,
        ) {
            Ok(p) => p,
            Err(message) => {
                return vec![HostCommand::SourceError {
                    request_id,
                    message,
                }];
            }
        };
        let headers = if plan.use_pcs_ua {
            self.session.pcs_api_headers()
        } else {
            self.session.web_api_headers()
        };
        if let Err(e) = self.start_http(
            state,
            "GET",
            plan.url,
            headers,
            true,
            Some(25_000),
            Some(request_id),
            HttpFlow::Browse {
                account_epoch: self.account_epoch,
                request_id,
                source_id,
                kind: plan.kind,
                folder_id,
                cursor,
                query,
                filters,
                retried_oauth,
            },
            host,
        ) {
            return vec![HostCommand::SourceError {
                request_id,
                message: format!("无法发起网盘目录请求：{}", error_message(e)),
            }];
        }
        Vec::new()
    }

    pub(super) fn handle_completed(
        &mut self,
        state: &mut WindowState,
        op: Id,
        result: Value,
        host: &mut impl Host,
        now_ms: u64,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let now_secs = now_ms / 1000;

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
            let (_, epoch, next_session, next_quota, close_modal, notify_login) =
                state.secret_write_op.take().unwrap();
            if epoch != self.account_epoch {
                return Ok(Vec::new());
            }
            if completed::<StoreWrite>(result).is_ok_and(|w| w.stored) {
                let mut cmds = self.commit_session(state, next_session, next_quota, host);
                self.request_quota(state, host);
                cmds.extend(self.sync_ui(state));
                cmds.extend(self.maybe_sync_sources());
                if close_modal {
                    cmds.push(HostCommand::CloseModal {
                        id: Some("baidupan.login".into()),
                    });
                }
                if notify_login {
                    cmds.extend(ui::refresh_all_sources());
                    let label = self.session.username.as_deref().unwrap_or("百度网盘用户");
                    cmds.push(ui::toast(format!("百度网盘登录成功：{label}")));
                }
                return Ok(cmds);
            }
            return Ok(vec![ui::toast("系统凭据存储未能写入，请稍后重试")]);
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
                    if matches!(
                        job.flow,
                        HttpFlow::QrConfirmLogin { .. }
                            | HttpFlow::ResolveOpenApiHead { .. }
                            | HttpFlow::SubtitleOpenApiHead { .. }
                    ) {
                        host.close(stream_handle);
                        return Ok(self.finish_http(state, job, host, now_ms, now_secs));
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
                            return Ok(self.fail_http_flow(state, &job.flow, e, host, now_secs));
                        }
                    }
                }
                Ok(_) => {
                    return Ok(self.fail_http_flow(
                        state,
                        &job.flow,
                        ErrorCode::BadState,
                        host,
                        now_secs,
                    ));
                }
                Err(e) => {
                    return Ok(self.fail_http_flow(state, &job.flow, e, host, now_secs));
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
                    return Ok(self.fail_http_flow(
                        state,
                        &job.flow,
                        ErrorCode::QuotaExceeded,
                        host,
                        now_secs,
                    ));
                }
                job.body.extend_from_slice(&chunk);
                if read.eof {
                    if let Some(s) = job.stream.take() {
                        host.close(s);
                    }
                    return Ok(self.finish_http(state, job, host, now_ms, now_secs));
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
                        Ok(self.fail_http_flow(state, &job.flow, e, host, now_secs))
                    }
                }
            }
            Err(e) => {
                if let Some(s) = job.stream.take() {
                    host.close(s);
                }
                Ok(self.fail_http_flow(state, &job.flow, e, host, now_secs))
            }
        }
    }

    pub(super) fn fail_http_flow(
        &self,
        state: &mut WindowState,
        flow: &HttpFlow,
        code: ErrorCode,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let denied_hint =
            "（请确认已授予插件网络权限，且代理未返回 198.18.0.0/15 等 Fake-IP 地址）";
        match flow {
            HttpFlow::QrPoll { manual, .. } if code == ErrorCode::Timeout => {
                if *manual {
                    vec![ui::toast(
                        "尚未检测到扫码，请用手机「百度网盘 App」扫描二维码",
                    )]
                } else {
                    Vec::new()
                }
            }
            HttpFlow::Browse { request_id, .. }
            | HttpFlow::ResolveLocate { request_id, .. }
            | HttpFlow::ResolveOpenApiMeta { request_id, .. }
            | HttpFlow::ResolveOpenApiHead { request_id, .. }
            | HttpFlow::ResolveStreaming { request_id, .. } => vec![HostCommand::SourceError {
                request_id: *request_id,
                message: if code == ErrorCode::Denied {
                    format!(
                        "百度网盘网络请求被拒绝：{}{denied_hint}",
                        error_message(code)
                    )
                } else {
                    format!("百度网盘网络请求失败 ({})", error_message(code))
                },
            }],
            HttpFlow::OAuthRefresh { retry, .. } => {
                if let Some(request_id) = retry.request_id() {
                    vec![HostCommand::SourceError {
                        request_id,
                        message: format!(
                            "刷新 OAuth 令牌失败 ({})，请重新登录",
                            error_message(code)
                        ),
                    }]
                } else {
                    vec![ui::toast(format!(
                        "刷新 OAuth 令牌失败：{}",
                        error_message(code)
                    ))]
                }
            }
            HttpFlow::QrGetSign { .. }
            | HttpFlow::QrDownloadPng { .. }
            | HttpFlow::QrConfirmLogin { .. }
            | HttpFlow::QrPoll { manual: true, .. }
            | HttpFlow::UserInfo {
                notify_on_success: true,
                ..
            } => vec![ui::toast(if code == ErrorCode::Denied {
                format!("百度账号请求被拒绝：{}{denied_hint}", error_message(code))
            } else {
                format!("百度账号请求失败：{}", error_message(code))
            })],
            HttpFlow::SubtitleLocate { .. }
            | HttpFlow::SubtitleOpenApiMeta { .. }
            | HttpFlow::SubtitleOpenApiHead { .. } => {
                self.pump_next_subtitle(state, host, now_secs)
            }
            _ => Vec::new(),
        }
    }

    fn finish_http(
        &mut self,
        state: &mut WindowState,
        job: ActiveHttp,
        host: &mut impl Host,
        now_ms: u64,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let is_binary_or_head = matches!(
            job.flow,
            HttpFlow::QrDownloadPng { .. }
                | HttpFlow::QrConfirmLogin { .. }
                | HttpFlow::ResolveOpenApiHead { .. }
                | HttpFlow::SubtitleOpenApiHead { .. }
        );
        let body = if is_binary_or_head {
            job.body
        } else {
            match sign::decode_http_body(&job.headers, &job.body, MAX_HTTP_BODY) {
                Ok(b) => b,
                Err(_) => {
                    return self.fail_http_flow(
                        state,
                        &job.flow,
                        ErrorCode::Unsupported,
                        host,
                        now_secs,
                    );
                }
            }
        };

        match job.flow {
            HttpFlow::Browse {
                account_epoch,
                request_id,
                source_id,
                kind,
                folder_id,
                cursor,
                query,
                filters,
                retried_oauth,
            } => {
                if account_epoch != self.account_epoch {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "账号状态已变更，请刷新目录".into(),
                    }];
                }
                let Ok(json) = serde_json::from_slice::<Value>(&body) else {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "百度网盘目录接口返回的 JSON 格式无效".into(),
                    }];
                };
                let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
                if !retried_oauth
                    && self.session.can_refresh_oauth()
                    && auth::is_token_expired_errno(errno)
                {
                    let retry = OAuthRetry::Browse {
                        request_id,
                        source_id: source_id.clone(),
                        folder_id: folder_id.clone(),
                        cursor: cursor.clone(),
                        query: query.clone(),
                        filters: filters.clone(),
                    };
                    if self
                        .start_oauth_refresh(state, &self.session, retry, host)
                        .is_ok()
                    {
                        return Vec::new();
                    }
                }
                match catalog::parse_browse_response(
                    &source_id,
                    &kind,
                    &json,
                    self.prefs.show_thumbnails,
                ) {
                    Ok(mut parsed) => {
                        parsed.page.folder_id = folder_id.clone();
                        if let Some(dir) = &parsed.cache_dir {
                            catalog::merge_folder_cache(
                                &mut state.folder_caches,
                                dir,
                                cursor.is_none(),
                                parsed.media_items,
                                parsed.subtitles,
                            );
                        }
                        catalog::dedup_browse_page(
                            &mut state.browse_seen,
                            &mut state.browse_cursors,
                            &source_id,
                            folder_id.as_deref(),
                            query.as_deref(),
                            &filters,
                            cursor.as_deref(),
                            &mut parsed.page,
                        );
                        let target = catalog::PageTarget {
                            account_epoch,
                            source: source_id,
                            folder: folder_id,
                            query,
                            filters,
                        };
                        let mut cache = catalog::PageCache::new(target, request_id, parsed.page);
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
            flow @ (HttpFlow::QrGetSign { .. }
            | HttpFlow::QrDownloadPng { .. }
            | HttpFlow::QrPoll { .. }
            | HttpFlow::QrConfirmLogin { .. }
            | HttpFlow::UserInfo { .. }
            | HttpFlow::Quota { .. }
            | HttpFlow::OAuthRefresh { .. }) => {
                self.finish_account_flow(state, flow, &job.headers, &body, host, now_ms, now_secs)
            }
            flow @ (HttpFlow::SubtitleLocate { .. }
            | HttpFlow::SubtitleOpenApiMeta { .. }
            | HttpFlow::SubtitleOpenApiHead { .. }) => {
                self.finish_subtitle_flow(state, flow, &job.headers, &body, host, now_secs)
            }
            resolve_flow => self.finish_resolve_flow(
                state,
                resolve_flow,
                &job.final_url,
                &job.headers,
                &body,
                host,
            ),
        }
    }
}
