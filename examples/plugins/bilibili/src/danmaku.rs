//! Bilibili XML danmaku (`/x/v1/dm/list.so?oid={cid}`) parser & windowed
//! `TimedCue` slicer, plus `/x/player/wbi/v2` BCC JSON -> WebVTT converter and
//! `view_points` chapter extractor.

use iris_plugin_sdk::api::{
    Color,
    v2::{
        WireU64,
        enhance::{Chapter, CueContent, CueLayout, OverlayWindow, TimedCue},
    },
};
use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};
use serde_json::Value;

/// Decodes raw deflate (`/x/v1/dm/list.so`), zlib deflate, or gzip HTTP payloads within `max_len`.
pub fn decode_http_body(
    headers: &[(String, String)],
    raw: &[u8],
    max_len: usize,
) -> Result<Vec<u8>, &'static str> {
    let enc = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_ascii_lowercase());
    match enc.as_deref() {
        None | Some("") | Some("identity") => Ok(raw.to_vec()),
        Some("deflate") => decompress_to_vec_with_limit(raw, max_len)
            .or_else(|_| decompress_to_vec_zlib_with_limit(raw, max_len))
            .map_err(|_| "deflate 解压失败或超过大小上限"),
        Some("gzip") | Some("x-gzip") => {
            let payload = strip_gzip_header(raw).ok_or("gzip 头格式无效")?;
            decompress_to_vec_with_limit(payload, max_len)
                .map_err(|_| "gzip 解压失败或超过大小上限")
        }
        _ => Err("不支持的 HTTP Content-Encoding"),
    }
}

fn strip_gzip_header(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return None;
    }
    let flags = data[3];
    let mut pos = 10usize;
    if flags & 0x04 != 0 {
        let xlen = u16::from_le_bytes([*data.get(pos)?, *data.get(pos + 1)?]) as usize;
        pos = pos.checked_add(2)?.checked_add(xlen)?;
    }
    if flags & 0x08 != 0 {
        pos = pos.checked_add(data.get(pos..)?.iter().position(|&b| b == 0)? + 1)?;
    }
    if flags & 0x10 != 0 {
        pos = pos.checked_add(data.get(pos..)?.iter().position(|&b| b == 0)? + 1)?;
    }
    if flags & 0x02 != 0 {
        pos = pos.checked_add(2)?;
    }
    let end = data.len().checked_sub(8)?;
    (pos <= end).then(|| &data[pos..end])
}

#[derive(Clone, Debug)]
pub struct DanmakuItem {
    pub id: String,
    pub start_ms: u64,
    pub duration_ms: u64,
    pub layout: CueLayout,
    pub size: u16,
    pub rgb: u32,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct SubtitleTrackMeta {
    pub id: String,
    pub lang: String,
    pub label: String,
    pub url: String,
}

pub fn parse_danmaku_xml(xml: &str) -> Vec<DanmakuItem> {
    let Ok(doc) = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 65_536,
        },
    ) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for (idx, node) in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "d")
        .enumerate()
    {
        if items.len() >= 6_000 {
            break;
        }
        let Some(p_attr) = node.attribute("p") else {
            continue;
        };
        let Some(raw_text) = node.text().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let mut fields = p_attr.split(',');
        let Some(time_secs) = fields
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.)
        else {
            continue;
        };
        let mode: u8 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        let font_size: u16 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(25);
        let rgb: u32 = fields
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0xFFFFFF)
            & 0xFF_FFFF;

        let (layout, duration_ms) = match mode {
            4 => (CueLayout::Bottom, 4_500u64),
            5 => (CueLayout::Top, 4_500u64),
            1..=3 => (CueLayout::Scroll, 8_000u64),
            _ => continue, // Skip advanced scripted mode 7/8/9 for clean viewport readability.
        };
        let clean = crate::auth::sanitize_text(raw_text, 240);
        if clean.is_empty() {
            continue;
        }
        items.push(DanmakuItem {
            id: format!("dm.{idx}"),
            start_ms: (time_secs * 1000.) as u64,
            duration_ms,
            layout,
            size: font_size.clamp(14, 34),
            rgb,
            text: clean,
        });
    }
    items.sort_by_key(|d| d.start_ms);
    items
}

/// Slices sorted danmaku strictly inside `[window.start_ms, window.end_ms]` so
/// `OverlayBank::submit` never rejects an `OutOfWindow` cue.
pub fn cues_for_window(
    items: &[DanmakuItem],
    window: &OverlayWindow,
    opacity_pct: u8,
) -> Vec<TimedCue> {
    let start_bound = window.start_ms.0;
    let end_bound = window.end_ms.0;
    if end_bound <= start_bound {
        return Vec::new();
    }
    let alpha = ((u32::from(opacity_pct.clamp(10, 100)) * 255) / 100) as u8;
    let first = items.partition_point(|d| d.start_ms < start_bound);
    let mut out = Vec::with_capacity(128);
    for item in &items[first..] {
        if item.start_ms >= end_bound || out.len() >= 240 {
            break;
        }
        let max_dur = end_bound.saturating_sub(item.start_ms);
        let dur = item.duration_ms.min(max_dur);
        if dur < 500 {
            continue;
        }
        let color = Color::Hex(format!("#{:06X}{alpha:02X}", item.rgb));
        out.push(TimedCue {
            id: item.id.clone(),
            start_ms: WireU64(item.start_ms),
            duration_ms: WireU64(dur),
            layout: item.layout.clone(),
            content: CueContent::Text {
                text: item.text.clone(),
                size: item.size,
                color,
                bold: false,
            },
        });
    }
    out
}

pub fn parse_player_v2_info(json: &Value) -> (Vec<Chapter>, Vec<SubtitleTrackMeta>) {
    let data = json.get("data").unwrap_or(&Value::Null);
    let mut chapters = Vec::new();
    if let Some(points) = data.get("view_points").and_then(Value::as_array) {
        for pt in points.iter().take(64) {
            let Some(from) = pt.get("from").and_then(Value::as_u64) else {
                continue;
            };
            let content = pt
                .get("content")
                .and_then(Value::as_str)
                .map(|s| crate::auth::sanitize_text(s, 120))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "章节".into());
            chapters.push(Chapter {
                start_ms: WireU64(from.saturating_mul(1000)),
                title: content,
            });
        }
    }

    let mut subtitles = Vec::new();
    if let Some(subs) = data
        .pointer("/subtitle/subtitles")
        .and_then(Value::as_array)
    {
        for (idx, sub) in subs.iter().take(8).enumerate() {
            let raw_url = [sub.get("subtitle_url"), sub.get("subtitle_url_v2")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::trim)
                .find(|s| !s.is_empty())
                .unwrap_or("");
            if raw_url.is_empty() {
                continue;
            }
            let url = if let Some(rest) = raw_url.strip_prefix("//") {
                format!("https://{rest}")
            } else if let Some(rest) = raw_url.strip_prefix("http://") {
                format!("https://{rest}")
            } else {
                raw_url.to_owned()
            };
            if !url.starts_with("https://") {
                continue;
            }
            let raw_lan = sub.get("lan").and_then(Value::as_str).unwrap_or("zh");
            let clean_id: String = raw_lan
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .map(|c| c.to_ascii_lowercase())
                .take(24)
                .collect();
            let clean_id = clean_id.trim_matches(|c| c == '-' || c == '_');
            let id = if clean_id.is_empty() {
                format!("bili-sub-{idx}")
            } else {
                format!("bili-{idx}-{clean_id}")
            };
            let label = sub
                .get("lan_doc")
                .and_then(Value::as_str)
                .map(|s| crate::auth::sanitize_text(s, 80))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::auth::sanitize_text(raw_lan, 32));
            let lang = normalize_subtitle_lang(raw_lan);
            let prio = subtitle_priority(raw_lan, &lang);
            subtitles.push((
                prio,
                SubtitleTrackMeta {
                    id,
                    lang,
                    label,
                    url,
                },
            ));
        }
    }
    subtitles.sort_by_key(|(prio, _)| *prio);
    let subtitles = subtitles.into_iter().map(|(_, s)| s).collect();
    (chapters, subtitles)
}

fn normalize_subtitle_lang(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "ai-zh" | "zh-cn" | "zh-hans" | "zh" | "chs" => "zh-CN".into(),
        "zh-hk" | "zh-tw" | "zh-hant" | "cht" => "zh-Hant".into(),
        "ai-en" | "en-us" | "en-gb" | "en" => "en".into(),
        "ai-ja" | "ja" | "jp" => "ja".into(),
        "ai-ko" | "ko" | "kr" => "ko".into(),
        _ => {
            let trimmed = lower.strip_prefix("ai-").unwrap_or(&lower);
            let clean: String = trimmed
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(16)
                .collect();
            if clean.is_empty() {
                "zh-CN".into()
            } else {
                clean
            }
        }
    }
}

fn subtitle_priority(raw_lan: &str, normalized_lang: &str) -> u8 {
    let is_ai = raw_lan.trim().to_ascii_lowercase().starts_with("ai-");
    match (normalized_lang, is_ai) {
        ("zh-CN", false) => 0,
        ("zh-CN", true) => 1,
        ("zh-Hant", false) => 2,
        ("zh-Hant", true) => 3,
        ("en", false) => 4,
        ("en", true) => 5,
        (_, false) => 6,
        (_, true) => 7,
    }
}

/// Converts Bilibili's BCC subtitle JSON (`{"body":[{"from":0.5,"to":2.1,"content":"..."}]}`)
/// into standard WebVTT text for `ArtifactFormat::WebVtt`.
pub fn bcc_json_to_webvtt(json_bytes: &[u8]) -> Option<String> {
    let parsed: Value = serde_json::from_slice(json_bytes).ok()?;
    let body = parsed.get("body")?.as_array()?;
    let mut vtt = String::from("WEBVTT\n\n");
    let mut written = 0usize;
    for cue in body.iter().take(5_000) {
        let (Some(from), Some(to), Some(raw_content)) = (
            cue.get("from").and_then(Value::as_f64),
            cue.get("to").and_then(Value::as_f64),
            cue.get("content").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !from.is_finite() || !to.is_finite() || to <= from {
            continue;
        }
        // WebVTT forbids blank lines inside a cue payload and host validation rejects NUL/overlong lines.
        let mut lines = Vec::new();
        for line in raw_content.replace('\r', "").lines() {
            let trimmed = line.trim().replace('\0', "").replace("-->", "->");
            if !trimmed.is_empty() {
                lines.push(if trimmed.len() > 2048 {
                    trimmed.chars().take(512).collect()
                } else {
                    trimmed
                });
            }
        }
        if lines.is_empty() {
            continue;
        }
        vtt.push_str(&format!(
            "{} --> {}\n{}\n\n",
            format_vtt_timestamp((from.max(0.) * 1000.).round() as u64),
            format_vtt_timestamp((to.max(0.) * 1000.).round() as u64),
            lines.join("\n")
        ));
        written += 1;
        if vtt.len() > 900 * 1024 {
            break;
        }
    }
    (written > 0).then_some(vtt)
}

fn format_vtt_timestamp(total_ms: u64) -> String {
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}
