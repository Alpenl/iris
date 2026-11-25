//! BiliSail-aligned `playurl` resolution for UGC, PGC (Bangumi/Cinema), and Live:
//! split-track DASH `.m4s` (`DirectHttp` video + `Attachment` audio), `upos` CDN
//! preference over MCDN/PCDN, `StreamVariant` quality/codec selection, and playlists.

mod dash;

use crate::auth::{Preferences, StoredSession, sanitize_text};
use dash::{build_dash_media, cdn_priority, collect_sorted_urls, trim_playlist, url_port_allowed};
use iris_plugin_sdk::api::{
    SourceEntryKind,
    v2::{
        source::{PlaylistEntry, ResolvedMedia},
        stream::{RemoteResource, StreamSource, StreamSpec},
    },
};
use serde_json::Value;

pub struct VideoMeta {
    pub bvid: String,
    pub aid: u64,
    pub cid: u64,
    pub ep_id: Option<u64>,
    pub title: String,
    pub source_id: String,
    pub entry_id: String,
    pub playlist: Vec<PlaylistEntry>,
}

pub fn parse_ugc_view(
    json: &Value,
    requested_cid: Option<u64>,
    requested_page: usize,
    source_id: &str,
) -> Result<VideoMeta, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("无法获取稿件信息");
        return Err(msg.to_owned());
    }
    let data = json.get("data").ok_or_else(|| "缺少视频详情".to_owned())?;
    let bvid = data
        .get("bvid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少 bvid".to_owned())?
        .to_owned();
    let aid = data.get("aid").and_then(Value::as_u64).unwrap_or(0);
    let base_title = data
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&bvid)
        .to_owned();
    let pages = data
        .get("pages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut chosen_cid = data.get("cid").and_then(Value::as_u64).unwrap_or(0);
    let mut part_title = String::new();
    if let Some(cid) = requested_cid {
        if let Some(p) = pages
            .iter()
            .find(|p| p.get("cid").and_then(Value::as_u64) == Some(cid))
        {
            chosen_cid = cid;
            part_title = p
                .get("part")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
    } else if let Some(p) = pages.get(requested_page.saturating_sub(1))
        && let Some(cid) = p.get("cid").and_then(Value::as_u64)
    {
        chosen_cid = cid;
        if pages.len() > 1 {
            part_title = p
                .get("part")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
    }
    if chosen_cid == 0 {
        return Err("未能解析视频分P cid".into());
    }

    let entry_id = if pages.len() > 1 {
        format!("ugc:{bvid}:{chosen_cid}")
    } else {
        bvid.clone()
    };
    let full_title = if part_title.is_empty() || part_title == base_title {
        base_title.clone()
    } else {
        format!("{base_title} · {part_title}")
    };

    let mut playlist = Vec::new();
    if pages.len() > 1 {
        let idx = pages
            .iter()
            .position(|p| p.get("cid").and_then(Value::as_u64) == Some(chosen_cid))
            .unwrap_or(0);
        let start = idx.saturating_sub(16);
        for p in pages.iter().skip(start).take(48) {
            if let Some(cid) = p.get("cid").and_then(Value::as_u64) {
                let no = p.get("page").and_then(Value::as_u64).unwrap_or(1);
                let part = p.get("part").and_then(Value::as_str).unwrap_or("");
                playlist.push(PlaylistEntry {
                    source_id: source_id.into(),
                    entry_id: format!("ugc:{bvid}:{cid}"),
                    title: if part.is_empty() {
                        format!("P{no}")
                    } else {
                        format!("P{no} · {part}")
                    },
                });
            }
        }
    } else if let Some(eps) = data
        .pointer("/ugc_season/sections/0/episodes")
        .and_then(Value::as_array)
    {
        let idx = eps
            .iter()
            .position(|e| e.get("bvid").and_then(Value::as_str) == Some(bvid.as_str()))
            .unwrap_or(0);
        let start = idx.saturating_sub(16);
        for ep in eps.iter().skip(start).take(48) {
            if let Some(ep_bv) = ep.get("bvid").and_then(Value::as_str) {
                playlist.push(PlaylistEntry {
                    source_id: source_id.into(),
                    entry_id: ep_bv.to_owned(),
                    title: ep
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(ep_bv)
                        .to_owned(),
                });
            }
        }
    }

    Ok(VideoMeta {
        bvid,
        aid,
        cid: chosen_cid,
        ep_id: None,
        title: full_title,
        source_id: source_id.into(),
        entry_id,
        playlist,
    })
}

pub fn parse_pgc_season(
    json: &Value,
    target_ep: Option<u64>,
    source_id: &str,
) -> Result<VideoMeta, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("无法获取番剧详情");
        return Err(msg.to_owned());
    }
    let result = json
        .get("result")
        .or_else(|| json.get("data"))
        .ok_or_else(|| "缺少番剧 result".to_owned())?;
    let season_title = result
        .get("season_title")
        .or_else(|| result.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("番剧");
    let eps = result
        .get("episodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "该番剧暂无可播放正片剧集".to_owned())?;
    if eps.is_empty() {
        return Err("该番剧暂无可播放剧集".into());
    }
    let chosen_idx = target_ep
        .and_then(|want| {
            eps.iter().position(|e| {
                e.get("ep_id")
                    .or_else(|| e.get("id"))
                    .and_then(Value::as_u64)
                    == Some(want)
            })
        })
        .unwrap_or(0);
    let ep = &eps[chosen_idx];
    let ep_id = ep
        .get("ep_id")
        .or_else(|| ep.get("id"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "缺少 ep_id".to_owned())?;
    let cid = ep
        .get("cid")
        .and_then(Value::as_u64)
        .ok_or_else(|| "缺少剧集 cid".to_owned())?;
    let aid = ep.get("aid").and_then(Value::as_u64).unwrap_or(0);
    let bvid = ep
        .get("bvid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let short = ep.get("title").and_then(Value::as_str).unwrap_or("");
    let long = ep.get("long_title").and_then(Value::as_str).unwrap_or("");
    let title = if long.is_empty() {
        format!("{season_title} · 第 {short} 话")
    } else {
        format!("{season_title} · {short} {long}")
    };

    let start = chosen_idx.saturating_sub(16);
    let mut playlist = Vec::new();
    for item in eps.iter().skip(start).take(48) {
        if let Some(id) = item
            .get("ep_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_u64)
        {
            let s = item.get("title").and_then(Value::as_str).unwrap_or("");
            let l = item.get("long_title").and_then(Value::as_str).unwrap_or("");
            playlist.push(PlaylistEntry {
                source_id: source_id.into(),
                entry_id: format!("ep:{id}"),
                title: if l.is_empty() {
                    format!("第 {s} 话")
                } else {
                    format!("{s} · {l}")
                },
            });
        }
    }

    Ok(VideoMeta {
        bvid,
        aid,
        cid,
        ep_id: Some(ep_id),
        title,
        source_id: source_id.into(),
        entry_id: format!("ep:{ep_id}"),
        playlist,
    })
}

pub fn build_vod_media(
    json: &Value,
    meta: &VideoMeta,
    requested_variant: Option<&str>,
    prefs: &Preferences,
    session: &StoredSession,
) -> Result<ResolvedMedia, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("无法获取播放地址");
        return Err(format!("获取播放流失败：{msg}"));
    }
    let root = json
        .pointer("/result/video_info")
        .or_else(|| json.get("result"))
        .or_else(|| json.get("data"))
        .ok_or_else(|| "播放响应缺少数据节点".to_owned())?;

    let headers = session.media_headers(false);
    if let Some(dash) = root.get("dash") {
        return build_dash_media(dash, meta, requested_variant, prefs, &headers);
    }
    if let Some(durl) = root
        .get("durl")
        .or_else(|| root.get("durls"))
        .and_then(Value::as_array)
        && let Some(first) = durl.first()
    {
        let urls = collect_sorted_urls(first);
        if urls.is_empty() {
            return Err("durl 未包含有效媒体地址".into());
        }
        let variants: Vec<iris_plugin_sdk::api::StreamVariant> = urls
            .iter()
            .take(6)
            .enumerate()
            .map(|(i, _)| iris_plugin_sdk::api::StreamVariant {
                id: format!("line.{i}"),
                label: format!("MP4 · 线路 {}", i + 1),
            })
            .collect();
        let chosen_idx = requested_variant
            .and_then(|v| v.strip_prefix("line."))
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&i| i < urls.len().min(6))
            .unwrap_or(0);
        let clean_title = sanitize_text(&meta.title, 400);
        return Ok(ResolvedMedia {
            stream: StreamSpec {
                source_id: (!meta.source_id.is_empty()).then(|| meta.source_id.clone()),
                entry_id: (!meta.entry_id.is_empty()).then(|| meta.entry_id.clone()),
                title: Some(if clean_title.is_empty() {
                    meta.bvid.clone()
                } else {
                    clean_title
                }),
                kind: Some(SourceEntryKind::Video),
                is_live: false,
                source: StreamSource::DirectHttp {
                    primary: RemoteResource {
                        url: urls[chosen_idx].clone(),
                        headers,
                        origin_grant: None,
                    },
                    fallback_urls: Vec::new(),
                    identity: None,
                },
                variants: if variants.len() > 1 {
                    variants
                } else {
                    Vec::new()
                },
                active_variant: (urls.len() > 1).then(|| format!("line.{chosen_idx}")),
                audio_tracks: Vec::new(),
                subtitles: Vec::new(),
            },
            playlist: trim_playlist(&meta.playlist),
        });
    }
    Err("未找到可播放的 DASH 或 MP4 流（可能需要大会员或地区权限）".into())
}

pub fn build_live_media(
    json: &Value,
    room_id: u64,
    requested_variant: Option<&str>,
    fallback_title: Option<&str>,
    source_id: &str,
    session: &StoredSession,
) -> Result<ResolvedMedia, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("无法获取直播间流信息");
        return Err(msg.to_owned());
    }
    let data = json
        .get("data")
        .ok_or_else(|| "直播响应缺少 data".to_owned())?;
    if data.get("live_status").and_then(Value::as_u64) != Some(1) {
        return Err("该主播当前未开播".into());
    }
    let streams = data
        .pointer("/playurl_info/playurl/stream")
        .and_then(Value::as_array)
        .ok_or_else(|| "未返回可用直播流".to_owned())?;

    let mut hls_urls = Vec::new();
    let mut flv_urls = Vec::new();
    for proto in streams {
        let pname = proto
            .get("protocol_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(formats) = proto.get("format").and_then(Value::as_array) else {
            continue;
        };
        for fmt in formats {
            let Some(codecs) = fmt.get("codec").and_then(Value::as_array) else {
                continue;
            };
            for c in codecs {
                let base_url = c.get("base_url").and_then(Value::as_str).unwrap_or("");
                if base_url.is_empty() {
                    continue;
                }
                if let Some(infos) = c.get("url_info").and_then(Value::as_array) {
                    for info in infos {
                        let host = info.get("host").and_then(Value::as_str).unwrap_or("");
                        let extra = info.get("extra").and_then(Value::as_str).unwrap_or("");
                        if host.starts_with("http://") || host.starts_with("https://") {
                            let full = format!("{host}{base_url}{extra}");
                            if pname == "http_hls" {
                                hls_urls.push(full);
                            } else {
                                flv_urls.push(full);
                            }
                        }
                    }
                }
            }
        }
    }
    if hls_urls.iter().any(|u| url_port_allowed(u)) {
        hls_urls.retain(|u| url_port_allowed(u));
    }
    hls_urls.sort_by_key(|u| cdn_priority(u));
    hls_urls.dedup();
    if flv_urls.iter().any(|u| url_port_allowed(u)) {
        flv_urls.retain(|u| url_port_allowed(u));
    }
    flv_urls.sort_by_key(|u| cdn_priority(u));
    flv_urls.dedup();
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    for (i, u) in hls_urls.into_iter().take(4).enumerate() {
        candidates.push((format!("hls.{i}"), format!("HLS · 线路 {}", i + 1), u));
    }
    for (i, u) in flv_urls.into_iter().take(2).enumerate() {
        candidates.push((format!("flv.{i}"), format!("FLV · 线路 {}", i + 1), u));
    }
    if candidates.is_empty() {
        return Err("未解析到直播播放地址".into());
    }
    let chosen_idx = requested_variant
        .and_then(|want| candidates.iter().position(|(id, _, _)| id == want))
        .unwrap_or(0);
    let variants: Vec<iris_plugin_sdk::api::StreamVariant> = if candidates.len() > 1 {
        candidates
            .iter()
            .map(|(id, label, _)| iris_plugin_sdk::api::StreamVariant {
                id: id.clone(),
                label: label.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let active_variant = (candidates.len() > 1).then(|| candidates[chosen_idx].0.clone());
    let headers = session.media_headers(false);
    Ok(ResolvedMedia {
        stream: StreamSpec {
            source_id: (!source_id.is_empty()).then(|| source_id.into()),
            entry_id: Some(format!("live:{room_id}")),
            title: Some(
                fallback_title
                    .map(|s| sanitize_text(s, 400))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("哔哩直播 · {room_id}")),
            ),
            kind: Some(SourceEntryKind::Video),
            is_live: true,
            source: StreamSource::DirectHttp {
                primary: RemoteResource {
                    url: candidates[chosen_idx].2.clone(),
                    headers,
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
        playlist: Vec::new(),
    })
}

pub fn qn_label(qn: u32) -> &'static str {
    match qn {
        127 => "8K 超高清",
        126 => "杜比视界",
        125 => "HDR 真彩",
        120 => "4K 超清",
        116 => "1080P 60帧",
        112 => "1080P 高码率",
        80 => "1080P 高清",
        74 => "720P 60帧",
        64 => "720P 高清",
        32 => "480P 清晰",
        16 => "360P 流畅",
        _ => "自动画质",
    }
}
