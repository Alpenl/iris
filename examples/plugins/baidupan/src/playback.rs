//! Baidu Netdisk direct playback resolution:
//! - Cookie mode: PCS `method=locatedownload` v4 signed CDN links (`BaiduPCS-Rust` / `BaiduPCS-Go`)
//! - OpenAPI mode: Official `xpan/multimedia?method=filemetas` + 302 `Location`, DLNA `filemetas`,
//!   and DLNA `mediainfo` (`OpenList`)
//!
//! Cloud HLS transcoding & `VirtualHls` compilation live in `playback/hls.rs`.

mod hls;

pub use hls::{
    StreamingParse, build_hls_resolved_media, build_streaming_url, compile_m3u8_to_virtual_hls,
    hls_variants, parse_streaming_response,
};

use crate::{
    auth::{
        BAIDU_APP_ID, CRACK_USER_AGENT, MOBILE_PCS_USER_AGENT, OFFICIAL_DLINK_USER_AGENT,
        Preferences,
    },
    media::MediaType,
    sign::{LocateSign, build_query, encode_query_component, sanitize_text},
};
use iris_plugin_sdk::api::{
    StreamVariant,
    v2::{
        source::{PlaylistEntry, ResolvedMedia},
        stream::{RemoteResource, StreamSource, StreamSpec},
    },
};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ResolveTarget {
    pub fs_id: u64,
    pub path: String,
    pub name: String,
    pub kind: MediaType,
    pub source_id: String,
    pub entry_id: String,
    pub playlist: Vec<PlaylistEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenApiLinkKind {
    Official,
    Crack,
    CrackVideo,
}

impl OpenApiLinkKind {
    pub fn from_pref(pref: &str, kind: MediaType) -> Self {
        match pref {
            "crack" => Self::Crack,
            "crack_video" if kind == MediaType::Video => Self::CrackVideo,
            "crack_video" => Self::Crack,
            _ => Self::Official,
        }
    }
}

pub enum ResolveStrategy {
    DirectOriginal { preferred_line: usize },
    TranscodeHls { profile: String },
}

pub fn choose_strategy(
    kind: MediaType,
    variant: Option<&str>,
    prefs: &Preferences,
) -> ResolveStrategy {
    if kind != MediaType::Video {
        let line = variant
            .and_then(|v| v.strip_prefix("orig."))
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        return ResolveStrategy::DirectOriginal {
            preferred_line: line,
        };
    }
    if let Some(v) = variant {
        if let Some(profile) = v.strip_prefix("hls.")
            && is_supported_hls_profile(profile)
        {
            return ResolveStrategy::TranscodeHls {
                profile: profile.to_owned(),
            };
        }
        if let Some(line) = v
            .strip_prefix("orig.")
            .and_then(|s| s.parse::<usize>().ok())
        {
            return ResolveStrategy::DirectOriginal {
                preferred_line: line,
            };
        }
    }
    match prefs.playback_mode.as_str() {
        "transcode_1080" => ResolveStrategy::TranscodeHls {
            profile: "M3U8_AUTO_1080".into(),
        },
        "transcode_720" => ResolveStrategy::TranscodeHls {
            profile: "M3U8_AUTO_720".into(),
        },
        "transcode_480" => ResolveStrategy::TranscodeHls {
            profile: "M3U8_AUTO_480".into(),
        },
        _ => ResolveStrategy::DirectOriginal { preferred_line: 0 },
    }
}

pub fn is_supported_hls_profile(profile: &str) -> bool {
    matches!(
        profile,
        "M3U8_AUTO_1080" | "M3U8_AUTO_720" | "M3U8_AUTO_480" | "M3U8_AUTO_4K"
    )
}

pub fn build_locate_download_url(path: &str, uid: u64, bduss: &str, now_secs: u64) -> String {
    let sign = LocateSign::new(uid, bduss, now_secs);
    format!(
        "https://pcs.baidu.com/rest/2.0/pcs/file?ant=1&check_blue=1&es=1&esl=1&app_id={BAIDU_APP_ID}&method=locatedownload&path={}&ver=4.0&clienttype=17&channel=0&apn_id=1_0&freeisp=0&queryfree=0&use=0&{}",
        encode_query_component(path),
        sign.url_params()
    )
}

pub fn build_openapi_meta_url(
    target: &ResolveTarget,
    access_token: &str,
    api_kind: OpenApiLinkKind,
) -> Result<String, String> {
    match api_kind {
        OpenApiLinkKind::Official => {
            if target.fs_id == 0 {
                return Err("该条目缺少 fs_id，请改用 DLNA 模式或从目录重新进入".into());
            }
            let params = vec![
                ("method", "filemetas".into()),
                ("access_token", access_token.to_owned()),
                ("fsids", format!("[{}]", target.fs_id)),
                ("dlink", "1".into()),
            ];
            Ok(format!(
                "https://pan.baidu.com/rest/2.0/xpan/multimedia?{}",
                build_query(&params)
            ))
        }
        OpenApiLinkKind::Crack => {
            let encoded_path =
                serde_json::to_string(&[&target.path]).map_err(|_| "路径编码失败".to_owned())?;
            let params = vec![
                ("access_token", access_token.to_owned()),
                ("target", encoded_path),
                ("dlink", "1".into()),
                ("web", "5".into()),
                ("origin", "dlna".into()),
            ];
            Ok(format!(
                "https://pan.baidu.com/api/filemetas?{}",
                build_query(&params)
            ))
        }
        OpenApiLinkKind::CrackVideo => {
            let params = vec![
                ("access_token", access_token.to_owned()),
                ("type", "VideoURL".into()),
                ("path", target.path.clone()),
                ("fs_id", target.fs_id.to_string()),
                ("devuid", "0%1".into()),
                ("clienttype", "1".into()),
                ("channel", "android_15_25010PN30C_bd-netdisk_1523a".into()),
                ("nom3u8", "1".into()),
                ("dlink", "1".into()),
                ("media", "1".into()),
                ("origin", "dlna".into()),
            ];
            Ok(format!(
                "https://pan.baidu.com/api/mediainfo?{}",
                build_query(&params)
            ))
        }
    }
}

/// Parses PCS `locatedownload` JSON and ranks usable CDN URLs (HTTPS & non-`nb.cache` first).
pub fn parse_locate_download_urls(json: &Value) -> Result<Vec<String>, (i64, String)> {
    let errno = json
        .get("errno")
        .or_else(|| json.get("error_code"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if errno != 0 {
        let msg = json
            .get("errmsg")
            .or_else(|| json.get("error_msg"))
            .and_then(Value::as_str)
            .unwrap_or("PCS Locate 取链失败");
        return Err((errno, sanitize_text(msg, 180)));
    }
    let Some(arr) = json.get("urls").and_then(Value::as_array) else {
        return Err((-1, "Locate 响应缺少 urls 字段".into()));
    };
    let mut urls: Vec<String> = arr
        .iter()
        .filter(|u| u.get("encrypt").and_then(Value::as_i64).unwrap_or(0) == 0)
        .filter_map(|u| u.get("url").and_then(Value::as_str))
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(str::to_owned)
        .collect();
    urls.sort_by_key(|u| cdn_rank(u));
    urls.dedup();
    if urls.is_empty() {
        return Err((-1, "未找到可用的未加密直链地址".into()));
    }
    Ok(urls)
}

pub fn cdn_rank(url: &str) -> (u8, u8) {
    let is_nb_cache =
        u8::from(url.starts_with("http://nb.cache") || url.starts_with("https://nb.cache"));
    let is_http = u8::from(!url.starts_with("https://"));
    (is_nb_cache, is_http)
}

pub fn parse_openapi_dlink(
    json: &Value,
    api_kind: OpenApiLinkKind,
) -> Result<String, (i64, String)> {
    let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
    if errno != 0 && !(errno == 31023 && api_kind == OpenApiLinkKind::CrackVideo) {
        let msg = json
            .get("errmsg")
            .and_then(Value::as_str)
            .unwrap_or("开放平台取链失败");
        return Err((errno, sanitize_text(msg, 180)));
    }
    let raw = match api_kind {
        OpenApiLinkKind::Official => json.pointer("/list/0/dlink").and_then(Value::as_str),
        OpenApiLinkKind::Crack => json.pointer("/info/0/dlink").and_then(Value::as_str),
        OpenApiLinkKind::CrackVideo => json.pointer("/info/dlink").and_then(Value::as_str),
    };
    raw.filter(|s| s.starts_with("https://") || s.starts_with("http://"))
        .map(str::to_owned)
        .ok_or_else(|| (-1, "未在开放平台响应中找到有效 dlink".into()))
}

pub fn build_direct_resolved_media(
    target: &ResolveTarget,
    urls: &[String],
    preferred_line: usize,
    user_agent: &str,
) -> Result<ResolvedMedia, String> {
    if urls.is_empty() {
        return Err("没有可用的播放地址".into());
    }
    let line_count = urls.len().min(3);
    let chosen = preferred_line.min(line_count - 1);
    let mut variants = Vec::new();
    for i in 0..line_count {
        let label = if line_count == 1 {
            "原画直链".into()
        } else {
            format!("原画 · 线路 {}", i + 1)
        };
        variants.push(StreamVariant {
            id: format!("orig.{i}"),
            label,
        });
    }
    if target.kind == MediaType::Video {
        variants.extend(hls_variants());
    } else if variants.len() <= 1 {
        variants.clear();
    }
    let active_variant = (!variants.is_empty()).then(|| format!("orig.{chosen}"));

    let title = sanitize_text(&target.name, 400);
    let mut media = ResolvedMedia {
        stream: StreamSpec {
            source_id: (!target.source_id.is_empty()).then(|| target.source_id.clone()),
            entry_id: Some(target.entry_id.clone()),
            title: Some(if title.is_empty() {
                "百度网盘媒体".into()
            } else {
                title
            }),
            kind: Some(target.kind.to_entry_kind()),
            is_live: false,
            source: StreamSource::DirectHttp {
                primary: RemoteResource {
                    url: urls[chosen].clone(),
                    headers: vec![("User-Agent".into(), user_agent.to_owned())],
                    origin_grant: None,
                },
                fallback_urls: Vec::new(),
                identity: None,
            },
            variants,
            active_variant,
            audio_tracks: Vec::new(),
            subtitles: Vec::new(),
        },
        playlist: target.playlist.clone(),
    };
    trim_playlist_to_budget(&mut media);
    Ok(media)
}

pub fn user_agent_for_openapi(api_kind: OpenApiLinkKind) -> &'static str {
    match api_kind {
        OpenApiLinkKind::Official => OFFICIAL_DLINK_USER_AGENT,
        OpenApiLinkKind::Crack | OpenApiLinkKind::CrackVideo => CRACK_USER_AGENT,
    }
}

pub fn user_agent_for_pcs_locate() -> &'static str {
    MOBILE_PCS_USER_AGENT
}

pub(crate) fn trim_playlist_to_budget(media: &mut ResolvedMedia) {
    while !media.playlist.is_empty()
        && serde_json::to_vec(media)
            .map(|v| v.len())
            .unwrap_or(usize::MAX)
            > 52 * 1024
    {
        media.playlist.pop();
    }
}
