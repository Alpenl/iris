//! Baidu cloud-transcoded HLS (`/api/streaming` & `/rest/2.0/xpan/file?method=streaming`),
//! `errno=133` `adToken` challenge parsing, and clean `VirtualHls` manifest compilation.

use super::{ResolveTarget, trim_playlist_to_budget};
use crate::{
    auth::{AuthMode, BAIDU_APP_ID, PAN_REFERER, StoredSession, WEB_USER_AGENT},
    sign::{build_query, sanitize_text},
};
use iris_plugin_sdk::api::{
    StreamVariant,
    v2::{
        CHUNK_LIMIT, ErrorCode, Id, WireU64,
        source::ResolvedMedia,
        stream::{
            ByteRange, HlsKey, HlsSegment, HlsTrack, ManifestFormat, RemoteResource,
            ResourceBinding, ResourceExtension, ResourceKind, ResourceSource, ResourceSpan,
            StreamSource, StreamSpec, TrackKind, VirtualHls,
        },
    },
};
use serde_json::Value;
use url::Url;

pub enum StreamingParse {
    NeedAdToken(String),
    VirtualHls(VirtualHls),
}

pub fn build_streaming_url(
    path: &str,
    profile: &str,
    session: &StoredSession,
    ad_token: Option<&str>,
) -> String {
    match session.mode {
        AuthMode::Cookie => {
            let vip = session.vip_type.unwrap_or(0);
            let mut params = vec![
                ("path", path.to_owned()),
                ("app_id", BAIDU_APP_ID.into()),
                ("clienttype", "0".into()),
                ("type", profile.to_owned()),
                ("vip", vip.to_string()),
                ("isplayer", "1".into()),
                ("check_blue", "1".into()),
            ];
            if let Some(token) = ad_token.filter(|s| !s.is_empty()) {
                params.push(("adToken", token.to_owned()));
            }
            format!(
                "https://pan.baidu.com/api/streaming?{}",
                build_query(&params)
            )
        }
        AuthMode::OpenApi => {
            let mut params = vec![
                ("method", "streaming".into()),
                ("access_token", session.access_token.clone()),
                ("path", path.to_owned()),
                ("type", profile.to_owned()),
            ];
            if let Some(token) = ad_token.filter(|s| !s.is_empty()) {
                params.push(("adToken", token.to_owned()));
            }
            format!(
                "https://pan.baidu.com/rest/2.0/xpan/file?{}",
                build_query(&params)
            )
        }
    }
}

pub fn parse_streaming_response(
    body: &[u8],
    base_url: &str,
) -> Result<StreamingParse, (i64, String)> {
    let text = std::str::from_utf8(body)
        .map_err(|_| (-1, "转码流响应不是有效 UTF-8".into()))?
        .trim_start_matches('\u{feff}')
        .trim();
    if text.starts_with('{') {
        let json: Value =
            serde_json::from_str(text).map_err(|_| (-1, "转码流 JSON 解析失败".into()))?;
        let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(-1);
        if errno == 133
            && let Some(ad_token) = json
                .get("adToken")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            return Ok(StreamingParse::NeedAdToken(ad_token.to_owned()));
        }
        let msg = json
            .get("errmsg")
            .or_else(|| json.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("该视频暂不支持云端转码或需要会员权限");
        return Err((errno, sanitize_text(msg, 180)));
    }
    if !text.starts_with("#EXTM3U") {
        return Err((-1, "转码流未返回有效的 M3U8 清单".into()));
    }
    let hls = compile_m3u8_to_virtual_hls(text, base_url).map_err(|msg| (-1, msg))?;
    Ok(StreamingParse::VirtualHls(hls))
}

pub fn compile_m3u8_to_virtual_hls(m3u8: &str, base_url: &str) -> Result<VirtualHls, String> {
    let base = Url::parse(base_url).map_err(|_| "无效的 M3U8 基址".to_owned())?;
    let seg_headers = vec![
        ("User-Agent".into(), WEB_USER_AGENT.into()),
        ("Referer".into(), PAN_REFERER.into()),
    ];

    let mut resources: Vec<ResourceBinding> = Vec::new();
    let mut segments: Vec<HlsSegment> = Vec::new();
    let mut sequence: u64 = 0;
    let mut discontinuity_sequence: u64 = 0;
    let mut pending_duration_ms: Option<u64> = None;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let mut pending_discontinuity = false;
    let mut current_init: Option<ResourceSpan> = None;
    let mut current_key: Option<(String, Option<String>)> = None;
    let mut ended = false;
    let mut key_idx = 0usize;
    let mut init_idx = 0usize;

    for raw_line in m3u8.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                sequence = n;
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                discontinuity_sequence = n;
            }
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let secs_str = rest.split(',').next().unwrap_or("").trim();
            if let Ok(secs) = secs_str.parse::<f64>()
                && secs.is_finite()
                && secs > 0.0
            {
                let ms = (secs * 1000.0).round().clamp(1.0, 86_400_000.0) as u64;
                pending_duration_ms = Some(ms);
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let (len_s, off_s) = rest
                .trim()
                .split_once('@')
                .map_or((rest.trim(), None), |(l, o)| (l, Some(o)));
            if let Ok(len) = len_s.parse::<u64>()
                && len > 0
            {
                let off = off_s.and_then(|o| o.parse::<u64>().ok());
                pending_range = Some((len, off));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            if rest.contains("METHOD=NONE") {
                current_key = None;
            } else if rest.contains("METHOD=AES-128")
                && let Some(uri) = extract_quoted_attr(rest, "URI")
                && let Ok(abs) = base.join(&uri)
            {
                let kid = format!("k{key_idx}");
                key_idx += 1;
                resources.push(ResourceBinding {
                    id: kid.clone(),
                    kind: ResourceKind::Key,
                    extension: ResourceExtension::Key,
                    size: Some(WireU64(16)),
                    source: ResourceSource::Remote {
                        request: RemoteResource {
                            url: abs.to_string(),
                            headers: seg_headers.clone(),
                            origin_grant: None,
                        },
                    },
                });
                let iv = extract_raw_attr(rest, "IV").and_then(|v| {
                    let hex = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X"))?;
                    (hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
                        .then(|| hex.to_ascii_lowercase())
                });
                current_key = Some((kid, iv));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = extract_quoted_attr(rest, "URI")
                && let Ok(abs) = base.join(&uri)
            {
                let mid = format!("m{init_idx}");
                init_idx += 1;
                resources.push(ResourceBinding {
                    id: mid.clone(),
                    kind: ResourceKind::Init,
                    extension: ResourceExtension::Mp4,
                    size: None,
                    source: ResourceSource::Remote {
                        request: RemoteResource {
                            url: abs.to_string(),
                            headers: seg_headers.clone(),
                            origin_grant: None,
                        },
                    },
                });
                let range = extract_quoted_attr(rest, "BYTERANGE").and_then(|br| {
                    let (l, o) = br.split_once('@')?;
                    Some(ByteRange {
                        len: WireU64(l.parse::<u64>().ok()?),
                        offset: WireU64(o.parse::<u64>().ok()?),
                    })
                });
                current_init = Some(ResourceSpan {
                    resource_id: mid,
                    range,
                });
            }
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if !line.starts_with('#') {
            let Ok(abs) = base.join(line) else {
                continue;
            };
            if !matches!(abs.scheme(), "http" | "https") {
                continue;
            }
            if resources.len() >= 9_500 {
                return Err("转码分片数超出单次清单上限，请切换「原画直链」播放".into());
            }
            let duration_ms = pending_duration_ms.take().unwrap_or(4000);
            let sid = format!("s{}", segments.len());
            let ext = if current_init.is_some() {
                ResourceExtension::M4s
            } else {
                ResourceExtension::Ts
            };
            resources.push(ResourceBinding {
                id: sid.clone(),
                kind: ResourceKind::Media,
                extension: ext,
                size: None,
                source: ResourceSource::Remote {
                    request: RemoteResource {
                        url: abs.to_string(),
                        headers: seg_headers.clone(),
                        origin_grant: None,
                    },
                },
            });
            let range = pending_range.take().map(|(len, off)| {
                let offset = off.unwrap_or_else(|| {
                    segments
                        .last()
                        .and_then(|s| s.resource.range.as_ref())
                        .map_or(0, |r| r.offset.0.saturating_add(r.len.0))
                });
                ByteRange {
                    offset: WireU64(offset),
                    len: WireU64(len),
                }
            });
            let seg_seq = sequence.saturating_add(segments.len() as u64);
            let key = current_key.as_ref().map(|(kid, iv)| HlsKey {
                resource_id: kid.clone(),
                iv: iv.clone().unwrap_or_else(|| format!("{seg_seq:032x}")),
            });
            segments.push(HlsSegment {
                resource: ResourceSpan {
                    resource_id: sid,
                    range,
                },
                duration_ms: WireU64(duration_ms),
                init: current_init.clone(),
                key,
                discontinuity: std::mem::take(&mut pending_discontinuity),
            });
        }
    }

    if segments.is_empty() {
        return Err("M3U8 清单中未解析到可播放分片".into());
    }

    Ok(VirtualHls {
        resources,
        tracks: vec![HlsTrack {
            id: "video".into(),
            kind: TrackKind::Video,
            group: None,
            label: "Baidu Transcode".into(),
            lang: None,
            codec: None,
            default: true,
            sequence: WireU64(sequence),
            discontinuity_sequence: WireU64(discontinuity_sequence),
            segments,
        }],
        variants: Vec::new(),
        live: !ended,
    })
}

pub fn build_hls_resolved_media(
    target: &ResolveTarget,
    profile: &str,
    hls: &VirtualHls,
    make_blob: impl FnOnce(&[u8]) -> Result<Id, ErrorCode>,
) -> Result<ResolvedMedia, String> {
    let encoded = serde_json::to_vec(hls).map_err(|_| "序列化 HLS 清单失败".to_owned())?;
    if encoded.len() > CHUNK_LIMIT {
        return Err("转码清单体积超过 1 MiB 上限，请切换为「原画直链」播放".into());
    }
    let doc_blob = make_blob(&encoded)
        .map_err(|e| format!("提交清单缓冲失败：{}", iris_plugin_sdk::error_message(e)))?;
    let mut variants = vec![StreamVariant {
        id: "orig.0".into(),
        label: "原画直链".into(),
    }];
    variants.extend(hls_variants());
    let active_id = format!("hls.{profile}");
    if !variants.iter().any(|v| v.id == active_id) {
        variants.push(StreamVariant {
            id: active_id.clone(),
            label: format!("云端转码 · {profile}"),
        });
    }
    let title = sanitize_text(&target.name, 400);
    let mut media = ResolvedMedia {
        stream: StreamSpec {
            source_id: (!target.source_id.is_empty()).then(|| target.source_id.clone()),
            entry_id: Some(target.entry_id.clone()),
            title: Some(if title.is_empty() {
                "百度网盘视频".into()
            } else {
                title
            }),
            kind: Some(target.kind.to_entry_kind()),
            is_live: hls.live,
            source: StreamSource::VirtualManifest {
                format: ManifestFormat::VirtualHls,
                document: doc_blob,
                context: None,
            },
            variants,
            active_variant: Some(active_id),
            audio_tracks: Vec::new(),
            subtitles: Vec::new(),
        },
        playlist: target.playlist.clone(),
    };
    trim_playlist_to_budget(&mut media);
    Ok(media)
}

pub fn hls_variants() -> Vec<StreamVariant> {
    vec![
        StreamVariant {
            id: "hls.M3U8_AUTO_1080".into(),
            label: "云端转码 · 1080P".into(),
        },
        StreamVariant {
            id: "hls.M3U8_AUTO_720".into(),
            label: "云端转码 · 720P".into(),
        },
        StreamVariant {
            id: "hls.M3U8_AUTO_480".into(),
            label: "云端转码 · 480P".into(),
        },
    ]
}

fn extract_quoted_attr(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn extract_raw_attr(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find(',').unwrap_or(rest.len());
    let val = rest[..end].trim().trim_matches('"');
    (!val.is_empty()).then(|| val.to_owned())
}
