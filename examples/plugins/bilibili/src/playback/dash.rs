//! Split-track DASH `.m4s` (`DirectHttp` video + `Attachment` audio) assembly and
//! `upos-*` CDN mirror selection over MCDN/PCDN nodes for Bilibili VOD.

use super::{VideoMeta, qn_label};
use crate::auth::{Preferences, sanitize_text};
use iris_plugin_sdk::api::{
    SourceEntryKind, StreamVariant,
    v2::{
        source::{PlaylistEntry, ResolvedMedia},
        stream::{Attachment, RemoteResource, StreamSource, StreamSpec},
    },
};
use serde_json::Value;

const ALLOWED_PORTS: &[u16] = &[
    80, 443, 4480, 4482, 4483, 8000, 8080, 8082, 8090, 8443, 9000, 9102,
];

pub(super) fn build_dash_media(
    dash: &Value,
    meta: &VideoMeta,
    requested_variant: Option<&str>,
    prefs: &Preferences,
    headers: &[(String, String)],
) -> Result<ResolvedMedia, String> {
    let videos = dash
        .get("video")
        .and_then(Value::as_array)
        .ok_or_else(|| "DASH 缺少视频轨".to_owned())?;
    if videos.is_empty() {
        return Err("DASH 视频轨为空".into());
    }

    struct VideoRep {
        variant_id: String,
        qn: u32,
        codec_tag: &'static str,
        bandwidth: u64,
        url: String,
    }

    let mut reps = Vec::new();
    for v in videos {
        let qn = v.get("id").and_then(Value::as_u64).unwrap_or(80) as u32;
        let codecid = v.get("codecid").and_then(Value::as_u64).unwrap_or(7);
        let codecs = v
            .get("codecs")
            .and_then(Value::as_str)
            .unwrap_or("avc1.640028");
        let codec_tag = match codecid {
            13 => "av1",
            12 => "hevc",
            _ if codecs.starts_with("hev") || codecs.starts_with("hvc") => "hevc",
            _ if codecs.starts_with("av01") => "av1",
            _ => "avc",
        };
        let Some(url) = collect_sorted_urls(v).into_iter().next() else {
            continue;
        };
        let bandwidth = v
            .get("bandwidth")
            .or_else(|| v.get("bandWidth"))
            .and_then(Value::as_u64)
            .unwrap_or(1_500_000)
            .max(1);
        let variant_id = format!("v.{qn}.{codec_tag}");
        if reps.iter().any(|r: &VideoRep| r.variant_id == variant_id) {
            continue;
        }
        reps.push(VideoRep {
            variant_id,
            qn,
            codec_tag,
            bandwidth,
            url,
        });
    }
    if reps.is_empty() {
        return Err("未能解析包含有效媒体地址的 DASH 视频流".into());
    }

    reps.sort_by(|a, b| {
        b.qn.cmp(&a.qn)
            .then_with(|| {
                codec_rank(b.codec_tag, &prefs.preferred_codec)
                    .cmp(&codec_rank(a.codec_tag, &prefs.preferred_codec))
            })
            .then_with(|| b.bandwidth.cmp(&a.bandwidth))
    });

    let chosen_idx = requested_variant
        .and_then(|want| reps.iter().position(|r| r.variant_id == want))
        .or_else(|| reps.iter().position(|r| r.qn <= prefs.preferred_qn))
        .unwrap_or(0);
    let chosen = &reps[chosen_idx];

    let mut listed: Vec<_> = reps.iter().take(16).collect();
    if chosen_idx >= 16 {
        listed[15] = chosen;
    }
    let variants: Vec<StreamVariant> = listed
        .into_iter()
        .map(|r| StreamVariant {
            id: r.variant_id.clone(),
            label: format!("{} · {}", qn_label(r.qn), r.codec_tag.to_uppercase()),
        })
        .collect();

    // Prefer reliable high-bitrate AAC as default, then preserve Hi-Res FLAC and
    // Dolby ahead of lower-bitrate AAC fallbacks so >=3 AAC tracks never crowd them out.
    let mut standard_aac: Vec<(String, &Value)> = Vec::new();
    if let Some(arr) = dash.get("audio").and_then(Value::as_array) {
        let mut standard: Vec<&Value> = arr.iter().collect();
        standard.sort_by_key(|a| {
            std::cmp::Reverse(
                a.get("bandwidth")
                    .or_else(|| a.get("bandWidth"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        });
        for a in standard {
            let bw_kbps = (a
                .get("bandwidth")
                .or_else(|| a.get("bandWidth"))
                .and_then(Value::as_u64)
                .unwrap_or(128_000)
                / 1000)
                .max(32);
            standard_aac.push((format!("{bw_kbps}kbps AAC"), a));
        }
    }
    let mut audio_candidates: Vec<(String, &Value)> = Vec::new();
    let mut aac_iter = standard_aac.into_iter();
    if let Some(best_aac) = aac_iter.next() {
        audio_candidates.push(best_aac);
    }
    if let Some(flac) = dash.pointer("/flac/audio").filter(|v| !v.is_null()) {
        audio_candidates.push(("Hi-Res 无损".into(), flac));
    }
    if let Some(dolby) = dash
        .pointer("/dolby/audio")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        audio_candidates.push(("杜比音效".into(), dolby));
    }
    audio_candidates.extend(aac_iter);

    let mut audio_tracks = Vec::new();
    for (label, node) in audio_candidates {
        let Some(url) = collect_sorted_urls(node).into_iter().next() else {
            continue;
        };
        if audio_tracks
            .iter()
            .any(|t: &Attachment| t.resource.url == url)
        {
            continue;
        }
        let idx = audio_tracks.len();
        audio_tracks.push(Attachment {
            id: format!("a{idx}"),
            label,
            lang: None,
            resource: RemoteResource {
                url,
                headers: headers.to_vec(),
                origin_grant: None,
            },
            default: idx == 0,
        });
        if audio_tracks.len() >= 3 {
            break;
        }
    }

    let title = sanitize_text(&meta.title, 400);
    Ok(ResolvedMedia {
        stream: StreamSpec {
            source_id: (!meta.source_id.is_empty()).then(|| meta.source_id.clone()),
            entry_id: (!meta.entry_id.is_empty()).then(|| meta.entry_id.clone()),
            title: Some(if title.is_empty() {
                meta.bvid.clone()
            } else {
                title
            }),
            kind: Some(SourceEntryKind::Video),
            is_live: false,
            source: StreamSource::DirectHttp {
                primary: RemoteResource {
                    url: chosen.url.clone(),
                    headers: headers.to_vec(),
                    origin_grant: None,
                },
                fallback_urls: Vec::new(),
                identity: None,
            },
            variants,
            active_variant: Some(chosen.variant_id.clone()),
            audio_tracks,
            subtitles: Vec::new(),
        },
        playlist: trim_playlist(&meta.playlist),
    })
}

pub(super) fn collect_sorted_urls(node: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    for key in ["baseUrl", "base_url", "url"] {
        if let Some(u) = node
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            urls.push(u.to_owned());
        }
    }
    for key in ["backupUrl", "backup_url"] {
        if let Some(arr) = node.get(key).and_then(Value::as_array) {
            for item in arr {
                if let Some(u) = item.as_str().filter(|s| !s.is_empty()) {
                    urls.push(u.to_owned());
                }
            }
        }
    }
    urls.retain(|u| u.starts_with("https://") || u.starts_with("http://"));
    if urls.iter().any(|u| url_port_allowed(u)) {
        urls.retain(|u| url_port_allowed(u));
    }
    urls.sort_by_key(|u| cdn_priority(u));
    urls.dedup();
    urls
}

fn url_port(url: &str) -> u16 {
    let default = if url.starts_with("https://") || url.starts_with("HTTPS://") {
        443
    } else {
        80
    };
    let authority = url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if let Some(after_bracket) = host_port.strip_prefix('[') {
        after_bracket
            .split_once("]:")
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(default)
    } else {
        host_port
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(default)
    }
}

pub(super) fn url_port_allowed(url: &str) -> bool {
    ALLOWED_PORTS.contains(&url_port(url))
}

/// Prefer stable `upos-*` & `akamaized` CDN mirrors on standard HTTPS port 443 over MCDN/PCDN nodes.
pub(super) fn cdn_priority(url: &str) -> u8 {
    let low = url.to_ascii_lowercase();
    let port = url_port(&low);
    if !ALLOWED_PORTS.contains(&port) {
        return 4;
    }
    let custom_port = port != 443 && port != 80;
    let is_pcdn = custom_port
        || low.contains("mcdn")
        || low.contains("pcdn")
        || low.contains("mountaintoys")
        || low.contains("szbdyd");
    if !is_pcdn && low.contains("upos-") {
        0
    } else if !is_pcdn && (low.contains("akamaized.net") || low.contains("bilivideo.com")) {
        1
    } else if !is_pcdn {
        2
    } else {
        3
    }
}

fn codec_rank(tag: &str, preferred: &str) -> u8 {
    if tag.eq_ignore_ascii_case(preferred) {
        3
    } else {
        match tag {
            "hevc" => 2,
            "avc" => 1,
            _ => 0,
        }
    }
}

pub(super) fn trim_playlist(items: &[PlaylistEntry]) -> Vec<PlaylistEntry> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for item in items.iter().take(48) {
        let mut clean = item.clone();
        clean.title = sanitize_text(&clean.title, 200);
        if clean.title.is_empty() {
            clean.title = clean.entry_id.clone();
        }
        let len = clean.source_id.len() + clean.entry_id.len() + clean.title.len() + 64;
        if bytes + len > 16 * 1024 {
            break;
        }
        bytes += len;
        out.push(clean);
    }
    out
}
