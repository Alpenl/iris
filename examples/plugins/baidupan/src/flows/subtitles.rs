//! Automatic sibling external subtitle (`.srt` / `.vtt` / `.ass`) discovery and attachment.

use crate::{
    BaiduPanPlugin, HttpFlow, WindowState,
    auth::{AuthMode, OFFICIAL_DLINK_USER_AGENT},
    catalog, media,
    playback::{self, OpenApiLinkKind, ResolveTarget},
    sign::{self, encode_query_component},
};
use iris_plugin_sdk::{
    api::v2::{
        Generation, HostCommand, Id, LogLevel, enhance::EnhancementResource, stream::RemoteResource,
    },
    client::Host,
    serde_json::{self, Value},
};

impl BaiduPanPlugin {
    pub(crate) fn pump_next_subtitle(
        &self,
        state: &mut WindowState,
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        if state.http_jobs.len() >= 6 {
            return Vec::new();
        }
        let Some((media, idx, sub)) = state.pending_subtitles.pop() else {
            return Vec::new();
        };
        if state.active_media != Some(media) {
            state.pending_subtitles.clear();
            return Vec::new();
        }
        match self.session.mode {
            AuthMode::Cookie => {
                if self.session.uid == 0 {
                    return Vec::new();
                }
                if now_secs == 0 {
                    state.pending_subtitles.clear();
                    return vec![HostCommand::Log {
                        level: LogLevel::Warn,
                        message: "baidupan: system wall clock unavailable (now_ms == 0); skipping Cookie subtitle signing".into(),
                    }];
                }
                let url = playback::build_locate_download_url(
                    &sub.path,
                    self.session.uid,
                    &self.session.bduss,
                    now_secs,
                );
                let _ = self.start_http(
                    state,
                    "POST",
                    url,
                    self.session.pcs_api_headers(),
                    true,
                    Some(20_000),
                    None,
                    HttpFlow::SubtitleLocate { media, idx, sub },
                    host,
                );
            }
            AuthMode::OpenApi => {
                let dummy = ResolveTarget {
                    fs_id: sub.fs_id,
                    path: sub.path.clone(),
                    name: sub.name.clone(),
                    kind: media::MediaType::Photo,
                    source_id: String::new(),
                    entry_id: String::new(),
                    playlist: Vec::new(),
                };
                let api_kind = OpenApiLinkKind::from_pref(
                    &self.prefs.openapi_link_api,
                    media::MediaType::Photo,
                );
                let Ok(url) =
                    playback::build_openapi_meta_url(&dummy, &self.session.access_token, api_kind)
                else {
                    return Vec::new();
                };
                let _ = self.start_http(
                    state,
                    "GET",
                    url,
                    self.session.web_api_headers(),
                    true,
                    Some(20_000),
                    None,
                    HttpFlow::SubtitleOpenApiMeta {
                        media,
                        idx,
                        sub,
                        api_kind,
                    },
                    host,
                );
            }
        }
        Vec::new()
    }

    pub(super) fn finish_subtitle_flow(
        &self,
        state: &mut WindowState,
        flow: HttpFlow,
        headers: &[(String, String)],
        body: &[u8],
        host: &mut impl Host,
        now_secs: u64,
    ) -> Vec<HostCommand> {
        match flow {
            HttpFlow::SubtitleLocate { media, idx, sub } => {
                let mut cmds = Vec::new();
                if state.active_media == Some(media)
                    && let Ok(json) = serde_json::from_slice::<Value>(body)
                    && let Ok(urls) = playback::parse_locate_download_urls(&json)
                    && let Some(url) = urls.into_iter().next()
                {
                    cmds.push(build_attach_subtitle_cmd(
                        media,
                        state.generation,
                        idx,
                        &sub,
                        url,
                        playback::user_agent_for_pcs_locate(),
                    ));
                }
                cmds.extend(self.pump_next_subtitle(state, host, now_secs));
                cmds
            }
            HttpFlow::SubtitleOpenApiMeta {
                media,
                idx,
                sub,
                api_kind,
            } => {
                if state.active_media == Some(media)
                    && let Ok(json) = serde_json::from_slice::<Value>(body)
                    && let Ok(dlink) = playback::parse_openapi_dlink(&json, api_kind)
                {
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
                                Some(15_000),
                                None,
                                HttpFlow::SubtitleOpenApiHead {
                                    media,
                                    idx,
                                    sub: sub.clone(),
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
                    let mut cmds = vec![build_attach_subtitle_cmd(
                        media,
                        state.generation,
                        idx,
                        &sub,
                        direct_url,
                        playback::user_agent_for_openapi(api_kind),
                    )];
                    cmds.extend(self.pump_next_subtitle(state, host, now_secs));
                    return cmds;
                }
                self.pump_next_subtitle(state, host, now_secs)
            }
            HttpFlow::SubtitleOpenApiHead {
                media,
                idx,
                sub,
                fallback_dlink,
            } => {
                let mut cmds = Vec::new();
                if state.active_media == Some(media) {
                    let loc = headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                        .map(|(_, v)| v.clone())
                        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                        .unwrap_or(fallback_dlink);
                    cmds.push(build_attach_subtitle_cmd(
                        media,
                        state.generation,
                        idx,
                        &sub,
                        loc,
                        OFFICIAL_DLINK_USER_AGENT,
                    ));
                }
                cmds.extend(self.pump_next_subtitle(state, host, now_secs));
                cmds
            }
            _ => Vec::new(),
        }
    }
}

fn build_attach_subtitle_cmd(
    media: Id,
    generation: Generation,
    idx: usize,
    sub: &catalog::CachedSubtitle,
    url: String,
    user_agent: &str,
) -> HostCommand {
    HostCommand::AttachSubtitle {
        media,
        generation,
        id: format!("bd-sub-{idx}"),
        label: sign::sanitize_text(&sub.name, 200),
        lang: sub.lang.clone(),
        format: sub.format,
        resource: EnhancementResource::Remote {
            request: RemoteResource {
                url,
                headers: vec![("User-Agent".into(), user_agent.to_owned())],
                origin_grant: None,
            },
        },
    }
}
