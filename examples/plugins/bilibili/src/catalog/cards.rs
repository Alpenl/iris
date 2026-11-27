//! Video/season/live card parsers, cover thumbnail helpers, and text formatting.

use crate::auth::{REFERER, USER_AGENT};
use iris_plugin_sdk::api::{
    SelectOption, SourceEntryKind,
    v2::{
        WireU64,
        source::{SourceEntry, SourceFilter},
        stream::RemoteResource,
    },
};
use serde_json::Value;

pub(super) fn parse_video_card(item: &Value) -> Option<SourceEntry> {
    let bvid = item
        .get("bvid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let title = strip_em(item.get("title").and_then(Value::as_str).unwrap_or(bvid));
    let owner = item
        .pointer("/owner/name")
        .or_else(|| item.get("author"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let views = item
        .pointer("/stat/view")
        .and_then(Value::as_u64)
        .or_else(|| item.get("play").and_then(Value::as_u64));
    let videos = item.get("videos").and_then(Value::as_u64).unwrap_or(1);
    let duration = item.get("duration").and_then(Value::as_f64).or_else(|| {
        item.get("duration")
            .and_then(Value::as_str)
            .and_then(parse_mm_ss)
    });
    let mut sub_parts = Vec::new();
    if !owner.is_empty() {
        sub_parts.push(owner.to_owned());
    }
    if let Some(v) = views {
        sub_parts.push(format!("{}播放", format_count(v)));
    }
    if videos > 1 {
        sub_parts.push(format!("{videos}P"));
    }
    Some(SourceEntry {
        id: if videos > 1 {
            format!("ugc:{bvid}")
        } else {
            bvid.to_owned()
        },
        name: title,
        kind: if videos > 1 {
            SourceEntryKind::Folder
        } else {
            SourceEntryKind::Video
        },
        size: None,
        modified_secs: item.get("pubdate").and_then(Value::as_u64).map(WireU64),
        duration,
        subtitle: (!sub_parts.is_empty()).then(|| sub_parts.join(" · ")),
        variants: Vec::new(),
        thumbnail: cover_resource(
            item.get("pic")
                .or_else(|| item.get("cover"))
                .and_then(Value::as_str),
        ),
    })
}

pub(super) fn parse_search_item(search_type: &str, item: &Value) -> Option<SourceEntry> {
    match search_type {
        "media_bangumi" | "media_ft" => parse_season_folder(item),
        "live_room" => {
            let room_id = item.get("roomid").and_then(Value::as_u64)?;
            let title = strip_em(
                item.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("直播间"),
            );
            let uname = item.get("uname").and_then(Value::as_str).unwrap_or("");
            Some(SourceEntry {
                id: format!("live:{room_id}"),
                name: title,
                kind: SourceEntryKind::Video,
                size: None,
                modified_secs: None,
                duration: None,
                subtitle: (!uname.is_empty()).then(|| uname.to_owned()),
                variants: Vec::new(),
                thumbnail: cover_resource(
                    item.get("user_cover")
                        .or_else(|| item.get("cover"))
                        .and_then(Value::as_str),
                ),
            })
        }
        _ => parse_video_card(item),
    }
}

pub(super) fn parse_season_folder(item: &Value) -> Option<SourceEntry> {
    let season_id = item.get("season_id").and_then(Value::as_u64)?;
    let title = strip_em(item.get("title").and_then(Value::as_str).unwrap_or("番剧"));
    let progress = item
        .get("new_ep")
        .and_then(|v| v.get("index_show"))
        .or_else(|| item.get("index_show"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let badge = item.get("badge").and_then(Value::as_str).unwrap_or("");
    let subtitle = match (badge.is_empty(), progress.is_empty()) {
        (false, false) => Some(format!("{badge} · {progress}")),
        (false, true) => Some(badge.to_owned()),
        (true, false) => Some(progress.to_owned()),
        (true, true) => None,
    };
    Some(SourceEntry {
        id: format!("ss:{season_id}"),
        name: title,
        kind: SourceEntryKind::Folder,
        size: None,
        modified_secs: None,
        duration: None,
        subtitle,
        variants: Vec::new(),
        thumbnail: cover_resource(item.get("cover").and_then(Value::as_str)),
    })
}

pub(super) fn bangumi_filter(selected: &str, authenticated: bool) -> SourceFilter {
    let mut options = Vec::new();
    if authenticated {
        options.push(opt("follow_1", "我的追番"));
        options.push(opt("follow_2", "我的追剧"));
    }
    options.push(opt("index_1", "热门番剧"));
    options.push(opt("index_4", "热门国创"));
    SourceFilter {
        id: "tab".into(),
        label: "分类".into(),
        selected: Some(selected.into()),
        options,
    }
}

pub(super) fn opt(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.into(),
        label: label.into(),
    }
}

pub fn cover_resource(raw: Option<&str>) -> Option<RemoteResource> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let normalized = if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else if let Some(rest) = raw.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        raw.to_owned()
    };
    if !normalized.starts_with("https://") || normalized.len() > 2048 {
        return None;
    }
    let compact = if normalized.contains("hdslb.com/bfs/") && !normalized.contains('@') {
        format!("{normalized}@320w_180h_1c.webp")
    } else {
        normalized
    };
    Some(RemoteResource {
        url: compact,
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), REFERER.into()),
        ],
        origin_grant: None,
    })
}

pub fn strip_em(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn parse_mm_ss(raw: &str) -> Option<f64> {
    let mut parts = raw.rsplit(':');
    let secs: f64 = parts.next()?.parse().ok()?;
    let mins: f64 = parts.next()?.parse().ok()?;
    let hours: f64 = parts.next().and_then(|h| h.parse().ok()).unwrap_or(0.);
    Some(hours * 3600. + mins * 60. + secs)
}

fn format_count(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.1}亿", n as f64 / 100_000_000.)
    } else if n >= 10_000 {
        format!("{:.1}万", n as f64 / 10_000.)
    } else {
        n.to_string()
    }
}
