//! Async task completions, HTTP body buffering, WebVTT artifact staging, and danmaku windows.

use super::*;
use iris_plugin_sdk::api::v2::{
    task::{CredentialBucket, Task},
    task_result::{HttpResult, ReadResult, StoreWrite},
};

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
                return vec![HostCommand::SourceError {
                    request_id,
                    message,
                }];
            }
        };
        if plan.needs_wbi && !retried_wbi {
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
        if let Err(e) = self.start_http(
            state,
            "GET",
            plan.url,
            true,
            Some(request_id),
            HttpFlow::Browse {
                request_id,
                source_id,
                kind: plan.kind,
                retried_wbi,
                query,
                folder_id,
                cursor,
                filters,
            },
            host,
        ) {
            return vec![HostCommand::SourceError {
                request_id,
                message: format!("无法发起目录请求：{e:?}"),
            }];
        }
        Vec::new()
    }

    pub(super) fn persist_prefs(&self, host: &mut impl Host) {
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
        if state
            .secret_write_op
            .as_ref()
            .is_some_and(|(id, _, _)| *id == op)
        {
            let (_, next_session, close_modal) = state.secret_write_op.take().unwrap();
            if completed::<StoreWrite>(result).is_ok_and(|w| w.stored) {
                self.session = next_session;
                state.qr = None;
                let _ = self.start_http(
                    state,
                    "GET",
                    "https://api.bilibili.com/x/web-interface/nav".into(),
                    true,
                    None,
                    HttpFlow::Nav { open_modal: false },
                    host,
                );
                let mut cmds = self.sync_ui(state);
                cmds.extend(self.maybe_sync_sources());
                if close_modal {
                    cmds.push(HostCommand::CloseModal {
                        id: Some("bili.login".into()),
                    });
                }
                cmds.extend(ui::refresh_all_sources());
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
                            return Ok(self.fail_http_flow(&job.flow, e));
                        }
                    }
                }
                Ok(_) => {
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return Ok(self.fail_http_flow(&job.flow, ErrorCode::BadState));
                }
                Err(e) => {
                    if matches!(job.flow, HttpFlow::SubtitleBcc { .. }) {
                        self.pump_next_subtitle(state, host);
                    }
                    return Ok(self.fail_http_flow(&job.flow, e));
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
                    return Ok(self.fail_http_flow(&job.flow, ErrorCode::QuotaExceeded));
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
                        Ok(self.fail_http_flow(&job.flow, e))
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
                Ok(self.fail_http_flow(&job.flow, e))
            }
        }
    }

    pub(super) fn fail_http_flow(&self, flow: &HttpFlow, code: ErrorCode) -> Vec<HostCommand> {
        let denied_hint = "（请确认已授予插件网络权限，且代理未对 *.bilibili.com 返回 198.18.0.0/15 或 2001:2::/48 等 Fake-IP）";
        match flow {
            HttpFlow::Browse { request_id, .. }
            | HttpFlow::ShortRedirect { request_id, .. }
            | HttpFlow::ResolveView { request_id, .. }
            | HttpFlow::ResolveSeason { request_id, .. }
            | HttpFlow::ResolveVodPlay { request_id, .. }
            | HttpFlow::ResolveLivePlay { request_id, .. } => vec![HostCommand::SourceError {
                request_id: *request_id,
                message: if code == ErrorCode::Denied {
                    format!("哔哩哔哩网络请求被拒绝：{code:?}{denied_hint}")
                } else {
                    format!("哔哩哔哩网络请求失败 ({code:?})")
                },
            }],
            HttpFlow::QrGenerate { .. } | HttpFlow::QrPoll { manual: true, .. } => {
                vec![ui::toast(if code == ErrorCode::Denied {
                    format!("通行证请求失败：{code:?}{denied_hint}")
                } else {
                    format!("通行证请求失败：{code:?}")
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
                    return self.fail_http_flow(&job.flow, ErrorCode::Unsupported);
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
                    return self.fail_http_flow(&other, ErrorCode::InvalidArgument);
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
            HttpFlow::Nav { open_modal } => {
                auth::apply_nav_response(json, &mut self.session, &mut self.wbi, now_secs);
                if state.needs_wbi_player_v2 && self.wbi.mixin().is_some() {
                    state.needs_wbi_player_v2 = false;
                    if let (Some(media), Some(cid)) = (state.active_media, state.active_cid) {
                        let (bvid, aid) = (state.active_bvid.clone(), state.active_aid);
                        self.request_player_v2(state, media, &bvid, aid, cid, true, host, now_secs);
                    }
                }
                let mut cmds = self.sync_ui(state);
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
            HttpFlow::QrPoll { open_modal, manual } => {
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
                                state.secret_write_op = Some((op, next_session, true));
                                Vec::new()
                            }
                            Err(e) => vec![ui::toast(format!("保存扫码凭据失败：{e:?}"))],
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
            HttpFlow::Browse {
                request_id,
                source_id,
                kind,
                retried_wbi,
                query,
                folder_id,
                cursor,
                filters,
            } => {
                if json.get("code").and_then(Value::as_i64) == Some(-403) && !retried_wbi {
                    self.wbi = WbiKeys::default();
                    return self.begin_browse(
                        state, request_id, source_id, folder_id, cursor, query, filters, true,
                        host, now_secs,
                    );
                }
                match catalog::parse_browse_response(
                    &source_id,
                    &kind,
                    json,
                    self.session.is_authenticated(),
                ) {
                    Ok(mut page) => {
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
            HttpFlow::ResolveView {
                request_id,
                source_id,
                requested_entry_id,
                cid,
                page,
                variant,
            } => match playback::parse_ugc_view(json, cid, page, &source_id) {
                Ok(meta) => self.request_vod_playurl(
                    state,
                    request_id,
                    requested_entry_id,
                    meta,
                    variant,
                    host,
                    now_secs,
                ),
                Err(message) => vec![HostCommand::SourceError {
                    request_id,
                    message,
                }],
            },
            HttpFlow::ResolveSeason {
                request_id,
                source_id,
                requested_entry_id,
                target_ep,
                variant,
            } => match playback::parse_pgc_season(json, target_ep, &source_id) {
                Ok(meta) => self.request_vod_playurl(
                    state,
                    request_id,
                    requested_entry_id,
                    meta,
                    variant,
                    host,
                    now_secs,
                ),
                Err(message) => vec![HostCommand::SourceError {
                    request_id,
                    message,
                }],
            },
            HttpFlow::ResolveVodPlay {
                request_id,
                requested_entry_id,
                meta,
                variant,
            } => {
                match playback::build_vod_media(
                    json,
                    &meta,
                    variant.as_deref(),
                    &self.prefs,
                    &self.session,
                ) {
                    Ok(media) => {
                        self.remember_cid(state, &meta, &requested_entry_id);
                        vec![HostCommand::ResolveStream {
                            request_id,
                            media: Box::new(media),
                        }]
                    }
                    Err(message) => vec![HostCommand::SourceError {
                        request_id,
                        message,
                    }],
                }
            }
            HttpFlow::ResolveLivePlay {
                request_id,
                room_id,
                source_id,
                variant,
            } => match playback::build_live_media(
                json,
                room_id,
                variant.as_deref(),
                None,
                &source_id,
                &self.session,
            ) {
                Ok(media) => {
                    state.last_resolved_cid = None;
                    vec![HostCommand::ResolveStream {
                        request_id,
                        media: Box::new(media),
                    }]
                }
                Err(message) => vec![HostCommand::SourceError {
                    request_id,
                    message,
                }],
            },
            HttpFlow::PlayerV2 {
                media,
                cid,
                retried_wbi,
            } => {
                if state.active_media != Some(media) || state.active_cid != Some(cid) {
                    return Vec::new();
                }
                if json.get("code").and_then(Value::as_i64) == Some(-403) && !retried_wbi {
                    self.wbi = WbiKeys::default();
                    state.needs_wbi_player_v2 = true;
                    let _ = self.start_http(
                        state,
                        "GET",
                        "https://api.bilibili.com/x/web-interface/nav".into(),
                        true,
                        None,
                        HttpFlow::Nav { open_modal: false },
                        host,
                    );
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

    #[allow(clippy::too_many_arguments)]
    fn request_vod_playurl(
        &self,
        state: &mut WindowState,
        request_id: Id,
        requested_entry_id: String,
        meta: VideoMeta,
        variant: Option<String>,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let qn = self.prefs.preferred_qn.to_string();
        let url = if let Some(ep_id) = meta.ep_id {
            format!(
                "https://api.bilibili.com/pgc/player/web/v2/playurl?ep_id={ep_id}&cid={}&qn={qn}&fnval=4048&fnver=0&fourk=1",
                meta.cid
            )
        } else {
            let mut params = vec![
                ("cid", meta.cid.to_string()),
                ("qn", qn),
                ("fnval", "4048".into()),
                ("fnver", "0".into()),
                ("fourk", "1".into()),
            ];
            if !meta.bvid.is_empty() {
                params.push(("bvid", meta.bvid.clone()));
            } else {
                params.push(("aid", meta.aid.to_string()));
            }
            let query = if let Some(mixin) = self.wbi.mixin() {
                wbi::sign_wbi_query(&params, &mixin, now_secs)
            } else {
                wbi::unsigned_query(&params)
            };
            format!("https://api.bilibili.com/x/player/wbi/playurl?{query}")
        };
        if let Err(e) = self.start_http(
            state,
            "GET",
            url,
            true,
            Some(request_id),
            HttpFlow::ResolveVodPlay {
                request_id,
                requested_entry_id,
                meta: Box::new(meta),
                variant,
            },
            host,
        ) {
            vec![HostCommand::SourceError {
                request_id,
                message: format!("无法请求播放流：{e:?}"),
            }]
        } else {
            Vec::new()
        }
    }
}
