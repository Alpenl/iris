//! Media playback resolution flows (PCS Locate v4, OpenAPI dlink/DLNA, and cloud HLS).

use crate::{
    BaiduPanPlugin, HttpFlow, OAuthRetry, WindowState,
    auth::{self, AuthMode, OFFICIAL_DLINK_USER_AGENT},
    catalog, media,
    playback::{self, OpenApiLinkKind, ResolveStrategy, ResolveTarget, StreamingParse},
    sign::encode_query_component,
};
use iris_plugin_sdk::{
    api::v2::{HostCommand, Id, LogLevel},
    client::Host,
    error_message,
    serde_json::{self, Value},
};

impl BaiduPanPlugin {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_resolve(
        &self,
        state: &mut WindowState,
        request_id: Id,
        source_id: &str,
        entry_id: &str,
        variant: Option<String>,
        retried_oauth: bool,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        if !self.session.is_authenticated() {
            return vec![HostCommand::SourceError {
                request_id,
                message: "请先登录百度网盘账号".into(),
            }];
        }
        let Some((fs_id, path)) = catalog::decode_entry_id(entry_id) else {
            return vec![HostCommand::SourceError {
                request_id,
                message: "无效的百度网盘文件标识".into(),
            }];
        };
        let parent = media::parent_dir(&path).unwrap_or_else(|| "/".into());
        let cached_item = state
            .folder_caches
            .get(&parent)
            .and_then(|c| c.media.iter().find(|m| m.path == path));
        let name = cached_item
            .map(|m| m.name.clone())
            .unwrap_or_else(|| media::basename(&path).to_owned());
        let kind = cached_item
            .map(|m| m.kind)
            .or_else(|| media::classify_file(&name, 0))
            .unwrap_or(media::MediaType::Video);
        let playlist = state
            .folder_caches
            .get(&parent)
            .map(|c| catalog::build_sibling_playlist(c, source_id, &path, kind))
            .unwrap_or_default();

        let resolved_fs_id =
            cached_item.map_or(fs_id, |m| if m.fs_id != 0 { m.fs_id } else { fs_id });
        let canonical_entry_id = catalog::encode_entry_id(resolved_fs_id, &path);

        let target = ResolveTarget {
            fs_id: resolved_fs_id,
            path,
            name,
            kind,
            source_id: source_id.to_owned(),
            entry_id: canonical_entry_id,
            playlist,
        };

        match playback::choose_strategy(target.kind, variant.as_deref(), &self.prefs) {
            ResolveStrategy::TranscodeHls { profile } => self.start_streaming_resolve(
                state,
                request_id,
                target,
                profile,
                variant,
                None,
                false,
                retried_oauth,
                host,
            ),
            ResolveStrategy::DirectOriginal { preferred_line } => self.start_direct_resolve(
                state,
                request_id,
                target,
                preferred_line,
                variant,
                retried_oauth,
                host,
                now_secs,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_direct_resolve(
        &self,
        state: &mut WindowState,
        request_id: Id,
        target: ResolveTarget,
        preferred_line: usize,
        original_variant: Option<String>,
        retried_oauth: bool,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let explicit_variant = original_variant.is_some();
        match self.session.mode {
            AuthMode::Cookie => {
                if self.session.uid == 0 {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "尚未获取到账号 UID，请先点击「刷新配额」或重新登录".into(),
                    }];
                }
                if now_secs == 0 {
                    return vec![
                        HostCommand::Log {
                            level: LogLevel::Error,
                            message: "baidupan: system wall clock unavailable (now_ms == 0); refusing to sign PCS Locate request".into(),
                        },
                        HostCommand::SourceError {
                            request_id,
                            message: "系统时钟不可用，无法生成百度网盘直链签名".into(),
                        },
                    ];
                }
                let url = playback::build_locate_download_url(
                    &target.path,
                    self.session.uid,
                    &self.session.bduss,
                    now_secs,
                );
                if let Err(e) = self.start_http(
                    state,
                    "POST",
                    url,
                    self.session.pcs_api_headers(),
                    true,
                    Some(25_000),
                    Some(request_id),
                    HttpFlow::ResolveLocate {
                        request_id,
                        target: Box::new(target),
                        preferred_line,
                        allow_hls_fallback: !explicit_variant,
                    },
                    host,
                ) {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: format!("无法发起 PCS Locate 取链请求：{}", error_message(e)),
                    }];
                }
                Vec::new()
            }
            AuthMode::OpenApi => {
                let api_kind =
                    OpenApiLinkKind::from_pref(&self.prefs.openapi_link_api, target.kind);
                let url = match playback::build_openapi_meta_url(
                    &target,
                    &self.session.access_token,
                    api_kind,
                ) {
                    Ok(u) => u,
                    Err(message) => {
                        return vec![HostCommand::SourceError {
                            request_id,
                            message,
                        }];
                    }
                };
                if let Err(e) = self.start_http(
                    state,
                    "GET",
                    url,
                    self.session.web_api_headers(),
                    true,
                    Some(25_000),
                    Some(request_id),
                    HttpFlow::ResolveOpenApiMeta {
                        request_id,
                        target: Box::new(target),
                        preferred_line,
                        original_variant,
                        api_kind,
                        retried_oauth,
                    },
                    host,
                ) {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: format!("无法发起开放平台取链请求：{}", error_message(e)),
                    }];
                }
                Vec::new()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_streaming_resolve(
        &self,
        state: &mut WindowState,
        request_id: Id,
        target: ResolveTarget,
        profile: String,
        original_variant: Option<String>,
        ad_token: Option<&str>,
        retried_ad_token: bool,
        retried_oauth: bool,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        let url = playback::build_streaming_url(&target.path, &profile, &self.session, ad_token);
        if let Err(e) = self.start_http(
            state,
            "GET",
            url,
            self.session.web_api_headers(),
            true,
            Some(25_000),
            Some(request_id),
            HttpFlow::ResolveStreaming {
                request_id,
                target: Box::new(target),
                profile,
                original_variant,
                retried_ad_token,
                retried_oauth,
            },
            host,
        ) {
            return vec![HostCommand::SourceError {
                request_id,
                message: format!("无法请求云端转码流：{}", error_message(e)),
            }];
        }
        Vec::new()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_resolve_flow(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        final_url: &str,
        headers: &[(String, String)],
        body: &[u8],
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        match flow {
            HttpFlow::ResolveLocate {
                request_id,
                target,
                preferred_line,
                allow_hls_fallback,
            } => {
                let Ok(json) = serde_json::from_slice::<Value>(body) else {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "PCS Locate 响应格式无效".into(),
                    }];
                };
                match playback::parse_locate_download_urls(&json) {
                    Ok(urls) => match playback::build_direct_resolved_media(
                        &target,
                        &urls,
                        preferred_line,
                        playback::user_agent_for_pcs_locate(),
                    ) {
                        Ok(media) => {
                            self.remember_video_target(state, &target);
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
                    Err((errno, msg)) => {
                        if allow_hls_fallback
                            && target.kind == media::MediaType::Video
                            && matches!(errno, 8002 | 9019 | 31326 | 31360 | 31362)
                        {
                            return self.start_streaming_resolve(
                                state,
                                request_id,
                                *target,
                                "M3U8_AUTO_1080".into(),
                                None,
                                None,
                                false,
                                false,
                                host,
                            );
                        }
                        vec![HostCommand::SourceError {
                            request_id,
                            message: format!("获取直链失败 (errno={errno})：{msg}"),
                        }]
                    }
                }
            }
            HttpFlow::ResolveOpenApiMeta {
                request_id,
                target,
                preferred_line,
                original_variant,
                api_kind,
                retried_oauth,
            } => {
                let Ok(json) = serde_json::from_slice::<Value>(body) else {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "开放平台取链响应格式无效".into(),
                    }];
                };
                match playback::parse_openapi_dlink(&json, api_kind) {
                    Ok(dlink) => {
                        let direct_url = if api_kind == OpenApiLinkKind::Official {
                            let with_token = format!(
                                "{dlink}&access_token={}",
                                encode_query_component(&self.session.access_token)
                            );
                            if self
                                .start_http(
                                    state,
                                    "HEAD",
                                    with_token.clone(),
                                    vec![("User-Agent".into(), OFFICIAL_DLINK_USER_AGENT.into())],
                                    false,
                                    Some(20_000),
                                    Some(request_id),
                                    HttpFlow::ResolveOpenApiHead {
                                        request_id,
                                        target: target.clone(),
                                        preferred_line,
                                        fallback_dlink: with_token.clone(),
                                    },
                                    host,
                                )
                                .is_ok()
                            {
                                return Vec::new();
                            }
                            with_token
                        } else {
                            dlink
                        };
                        let ua = playback::user_agent_for_openapi(api_kind);
                        match playback::build_direct_resolved_media(
                            &target,
                            &[direct_url],
                            preferred_line,
                            ua,
                        ) {
                            Ok(media) => {
                                self.remember_video_target(state, &target);
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
                    Err((errno, msg)) => {
                        if !retried_oauth
                            && self.session.can_refresh_oauth()
                            && auth::is_token_expired_errno(errno)
                        {
                            let retry = OAuthRetry::Resolve {
                                request_id,
                                source_id: target.source_id.clone(),
                                entry_id: target.entry_id.clone(),
                                variant: original_variant,
                            };
                            if self
                                .start_oauth_refresh(state, &self.session, retry, host)
                                .is_ok()
                            {
                                return Vec::new();
                            }
                        }
                        vec![HostCommand::SourceError {
                            request_id,
                            message: format!("开放平台取链失败 (errno={errno})：{msg}"),
                        }]
                    }
                }
            }
            HttpFlow::ResolveOpenApiHead {
                request_id,
                target,
                preferred_line,
                fallback_dlink,
            } => {
                let loc = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone())
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                    .unwrap_or(fallback_dlink);
                match playback::build_direct_resolved_media(
                    &target,
                    &[loc],
                    preferred_line,
                    OFFICIAL_DLINK_USER_AGENT,
                ) {
                    Ok(media) => {
                        self.remember_video_target(state, &target);
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
            HttpFlow::ResolveStreaming {
                request_id,
                target,
                profile,
                original_variant,
                retried_ad_token,
                retried_oauth,
            } => match playback::parse_streaming_response(body, final_url) {
                Ok(StreamingParse::NeedAdToken(ad_token)) if !retried_ad_token => self
                    .start_streaming_resolve(
                        state,
                        request_id,
                        *target,
                        profile,
                        original_variant,
                        Some(&ad_token),
                        true,
                        retried_oauth,
                        host,
                    ),
                Ok(StreamingParse::NeedAdToken(_)) => vec![HostCommand::SourceError {
                    request_id,
                    message: "云端转码流需要广告验证或会员权限，请尝试切换「原画直链」".into(),
                }],
                Ok(StreamingParse::VirtualHls(hls)) => {
                    match playback::build_hls_resolved_media(&target, &profile, &hls, |bytes| {
                        host.blob(bytes)
                    }) {
                        Ok(media) => {
                            self.remember_video_target(state, &target);
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
                Err((errno, msg)) => {
                    if !retried_oauth
                        && self.session.can_refresh_oauth()
                        && auth::is_token_expired_errno(errno)
                    {
                        let retry = OAuthRetry::Resolve {
                            request_id,
                            source_id: target.source_id.clone(),
                            entry_id: target.entry_id.clone(),
                            variant: original_variant,
                        };
                        if self
                            .start_oauth_refresh(state, &self.session, retry, host)
                            .is_ok()
                        {
                            return Vec::new();
                        }
                    }
                    vec![HostCommand::SourceError {
                        request_id,
                        message: format!("云端转码失败 (errno={errno})：{msg}"),
                    }]
                }
            },
            _ => Vec::new(),
        }
    }

    fn remember_video_target(&mut self, state: &mut WindowState, target: &ResolveTarget) {
        if target.kind != media::MediaType::Video {
            state.last_resolved_video = None;
            return;
        }
        let info = (target.path.clone(), target.name.clone());
        state.last_resolved_video = Some(info.clone());
        self.video_lookup.insert(target.entry_id.clone(), info);
    }
}
