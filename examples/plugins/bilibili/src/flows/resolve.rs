//! Playback target resolution flows (UGC view, PGC season, WBI playurl, and live).

use crate::{
    BilibiliPlugin, HttpFlow, PendingWbiPlayurl, WindowState,
    auth::WbiKeys,
    playback::{self, VideoMeta},
    wbi,
};
use iris_plugin_sdk::{
    api::v2::{HostCommand, Id, LogLevel},
    client::Host,
    error_message,
    serde_json::Value,
};

impl BilibiliPlugin {
    pub(super) fn finish_resolve_json_flow(
        &mut self,
        state: &mut WindowState,
        flow: HttpFlow,
        json: &Value,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
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
                    false,
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
                    false,
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
                retried_wbi,
                used_mixin,
            } => {
                if json.get("code").and_then(Value::as_i64) == Some(-403)
                    && meta.ep_id.is_none()
                    && !retried_wbi
                {
                    let current_mixin = self.wbi.mixin();
                    if current_mixin.is_some() && current_mixin != used_mixin {
                        return self.request_vod_playurl(
                            state,
                            request_id,
                            requested_entry_id,
                            *meta,
                            variant,
                            true,
                            host,
                            now_secs,
                        );
                    }
                    self.wbi = WbiKeys::default();
                    if let Err(e) = self.ensure_nav_in_flight(state, host) {
                        return vec![HostCommand::SourceError {
                            request_id,
                            message: format!("刷新 B站 WBI 签名密钥失败：{}", error_message(e)),
                        }];
                    }
                    if state.pending_wbi_playurl.len() < 8 {
                        state.pending_wbi_playurl.push(PendingWbiPlayurl {
                            request_id,
                            requested_entry_id,
                            meta: *meta,
                            variant,
                        });
                        return Vec::new();
                    }
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "待重试的 B站 解析请求过多，请稍后重试".into(),
                    }];
                }
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
            _ => Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_vod_playurl(
        &self,
        state: &mut WindowState,
        request_id: Id,
        requested_entry_id: String,
        meta: VideoMeta,
        variant: Option<String>,
        retried_wbi: bool,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        let qn = self.prefs.preferred_qn.to_string();
        let (url, used_mixin) = if let Some(ep_id) = meta.ep_id {
            (
                format!(
                    "https://api.bilibili.com/pgc/player/web/v2/playurl?ep_id={ep_id}&cid={}&qn={qn}&fnval=4048&fnver=0&fourk=1",
                    meta.cid
                ),
                None,
            )
        } else {
            if now_secs == 0 {
                return vec![
                    HostCommand::Log {
                        level: LogLevel::Error,
                        message: "bilibili: system wall clock unavailable (now_ms == 0); refusing to WBI-sign playurl request".into(),
                    },
                    HostCommand::SourceError {
                        request_id,
                        message: "系统时钟不可用，无法生成 B站 WBI 签名".into(),
                    },
                ];
            }
            let Some(mixin) = self.wbi.mixin() else {
                if retried_wbi {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: "获取 B站 WBI 签名密钥失败，请稍后重试".into(),
                    }];
                }
                if let Err(e) = self.ensure_nav_in_flight(state, host) {
                    return vec![HostCommand::SourceError {
                        request_id,
                        message: format!("刷新 B站 WBI 签名密钥失败：{}", error_message(e)),
                    }];
                }
                if state.pending_wbi_playurl.len() < 8 {
                    state.pending_wbi_playurl.push(PendingWbiPlayurl {
                        request_id,
                        requested_entry_id,
                        meta,
                        variant,
                    });
                    return Vec::new();
                }
                return vec![HostCommand::SourceError {
                    request_id,
                    message: "待重试的 B站 解析请求过多，请稍后重试".into(),
                }];
            };
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
            let query = wbi::sign_wbi_query(&params, &mixin, now_secs);
            (
                format!("https://api.bilibili.com/x/player/wbi/playurl?{query}"),
                Some(mixin),
            )
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
                retried_wbi,
                used_mixin,
            },
            host,
        ) {
            vec![HostCommand::SourceError {
                request_id,
                message: format!("无法请求播放流：{}", error_message(e)),
            }]
        } else {
            Vec::new()
        }
    }
}
