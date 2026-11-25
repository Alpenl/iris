//! Response parser converting Baidu Netdisk file list / category / search JSON
//! into sanitized Iris `SourcePage` entries, sort filters, thumbnails, and caches.

use super::{BrowseKind, CachedMediaItem, CachedSubtitle, PAGE_SIZE, encode_entry_id};
use crate::{
    auth::{PAN_REFERER, WEB_USER_AGENT},
    media,
    sign::{format_bytes, sanitize_text},
};
use iris_plugin_sdk::api::{
    SelectOption, SourceEntryKind,
    v2::{
        Id,
        source::{SourceEntry, SourceFilter, SourcePage},
        stream::RemoteResource,
    },
};
use serde_json::Value;
use std::collections::BTreeSet;

pub struct ParsedBrowse {
    pub page: SourcePage,
    pub cache_dir: Option<String>,
    pub media_items: Vec<CachedMediaItem>,
    pub subtitles: Vec<CachedSubtitle>,
}

pub fn parse_browse_response(
    source_id: &str,
    kind: &BrowseKind,
    json: &Value,
    show_thumbnails: bool,
) -> Result<ParsedBrowse, String> {
    let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
    if matches!(errno, -6 | 111 | 20016 | 20017) {
        return Ok(ParsedBrowse {
            page: SourcePage {
                source_id: source_id.into(),
                folder_id: None,
                parent_id: None,
                title: "需要重新登录百度网盘".into(),
                entries: Vec::new(),
                next_page: None,
                authenticated: false,
                auth_hint: Some(
                    "登录凭据已失效或未授权，请重新扫码、粘贴 Cookie 或更新令牌".into(),
                ),
                filters: Vec::new(),
            },
            cache_dir: None,
            media_items: Vec::new(),
            subtitles: Vec::new(),
        });
    }
    if errno != 0 {
        let msg = json
            .get("errmsg")
            .or_else(|| json.get("show_msg"))
            .and_then(Value::as_str)
            .unwrap_or("接口返回错误");
        return Err(format!(
            "百度网盘请求失败 (errno={errno})：{}",
            sanitize_text(msg, 180)
        ));
    }

    let raw_items: Vec<&Value> = json
        .get("list")
        .or_else(|| json.get("info"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            json.get("contentlist")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .collect();
    let raw_count = raw_items.len();

    let mut folders = Vec::new();
    let mut media_entries = Vec::new();
    let mut media_items = Vec::new();
    let mut subtitles = Vec::new();

    for item in raw_items {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .map(media::normalize_dir)
            .unwrap_or_default();
        if path.is_empty() || path == "/" {
            continue;
        }
        let fs_id = item.get("fs_id").and_then(Value::as_u64).unwrap_or(0);
        let raw_name = item
            .get("server_filename")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| media::basename(&path));
        let name = sanitize_text(raw_name, 240);
        if name.is_empty() {
            continue;
        }
        let is_dir = item.get("isdir").and_then(Value::as_i64) == Some(1);
        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
        let mtime = item
            .get("server_mtime")
            .or_else(|| item.get("local_mtime"))
            .and_then(Value::as_u64)
            .filter(|&t| t > 0);
        let category = item.get("category").and_then(Value::as_i64).unwrap_or(0);

        if is_dir {
            if matches!(kind, BrowseKind::CategoryList { .. })
                || matches!(
                    kind,
                    BrowseKind::Search {
                        category_filter: Some(_),
                        ..
                    }
                )
            {
                continue;
            }
            let sub = match kind {
                BrowseKind::Search { .. } => media::parent_dir(&path),
                _ => None,
            };
            folders.push(SourceEntry {
                id: path.clone(),
                name,
                kind: SourceEntryKind::Folder,
                size: None,
                modified_secs: mtime.map(Id),
                duration: None,
                subtitle: sub,
                variants: Vec::new(),
                thumbnail: None,
            });
            continue;
        }

        if let Some(sub_fmt) = media::subtitle_format_of(&name) {
            subtitles.push(CachedSubtitle {
                fs_id,
                path,
                name: name.clone(),
                format: sub_fmt,
                lang: media::infer_subtitle_lang(&name),
            });
            continue;
        }

        let Some(media_kind) = media::classify_file(&name, category) else {
            continue;
        };
        if let BrowseKind::Search {
            category_filter: Some(want),
            ..
        } = kind
            && media_kind != *want
        {
            continue;
        }

        let thumb = if show_thumbnails {
            extract_thumb(item)
        } else {
            None
        };
        let parent_hint = match kind {
            BrowseKind::DirList { .. } => None,
            BrowseKind::CategoryList { .. } | BrowseKind::Search { .. } => media::parent_dir(&path),
        };
        let subtitle = match (parent_hint, size > 0) {
            (Some(p), true) if p != "/" => Some(format!("{} · {}", format_bytes(size), p)),
            (Some(p), false) if p != "/" => Some(p),
            (_, true) => Some(format_bytes(size)),
            _ => None,
        };
        let entry_id = encode_entry_id(fs_id, &path);
        media_items.push(CachedMediaItem {
            fs_id,
            path,
            name: name.clone(),
            kind: media_kind,
            size,
        });
        media_entries.push(SourceEntry {
            id: entry_id,
            name,
            kind: media_kind.to_entry_kind(),
            size: (size > 0).then_some(Id(size)),
            modified_secs: mtime.map(Id),
            duration: None,
            subtitle,
            variants: Vec::new(),
            thumbnail: thumb,
        });
    }

    if let BrowseKind::DirList { order, desc, .. } = kind
        && order == "name"
    {
        folders.sort_by(|a, b| {
            let ord = media::natural_cmp(&a.name, &b.name);
            if *desc { ord.reverse() } else { ord }
        });
        media_entries.sort_by(|a, b| {
            let ord = media::natural_cmp(&a.name, &b.name);
            if *desc { ord.reverse() } else { ord }
        });
    }

    let mut entries = folders;
    entries.extend(media_entries);
    finalize_entries(&mut entries);

    let has_more_flag = json.get("has_more").and_then(Value::as_i64) == Some(1);
    let (title, folder_id, parent_id, filters, next_page, cache_dir) = match kind {
        BrowseKind::DirList {
            dir,
            order,
            desc,
            page_num,
        } => {
            let title = if dir == "/" {
                "百度网盘 · 全部文件".into()
            } else {
                sanitize_text(media::basename(dir), 200)
            };
            let next = (raw_count >= PAGE_SIZE as usize && *page_num < 200)
                .then(|| (page_num + 1).to_string());
            (
                title,
                Some(dir.clone()),
                media::parent_dir(dir),
                sort_filters(order, *desc),
                next,
                Some(dir.clone()),
            )
        }
        BrowseKind::CategoryList {
            category,
            order,
            desc,
            page_num,
        } => {
            let title = match category {
                1 => "百度网盘 · 全部视频",
                2 => "百度网盘 · 全部音频",
                _ => "百度网盘 · 全部照片",
            }
            .into();
            let next = ((raw_count >= PAGE_SIZE as usize || has_more_flag) && *page_num < 200)
                .then(|| (page_num + 1).to_string());
            (title, None, None, sort_filters(order, *desc), next, None)
        }
        BrowseKind::Search {
            keyword, page_num, ..
        } => {
            let next = ((has_more_flag || raw_count >= PAGE_SIZE as usize) && *page_num < 50)
                .then(|| (page_num + 1).to_string());
            (
                format!("搜索：{}", sanitize_text(keyword, 160)),
                None,
                None,
                Vec::new(),
                next,
                None,
            )
        }
    };

    Ok(ParsedBrowse {
        page: SourcePage {
            source_id: source_id.into(),
            folder_id,
            parent_id,
            title,
            entries,
            next_page,
            authenticated: true,
            auth_hint: None,
            filters,
        },
        cache_dir,
        media_items,
        subtitles,
    })
}

fn finalize_entries(entries: &mut Vec<SourceEntry>) {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(entries.len().min(60));
    let mut bytes = 0usize;
    for mut entry in entries.drain(..) {
        if entry.id.is_empty() || entry.id.len() > 2048 || !seen.insert(entry.id.clone()) {
            continue;
        }
        entry.name = sanitize_text(&entry.name, 240);
        if entry.name.is_empty() {
            entry.name = sanitize_text(&entry.id, 120);
        }
        entry.subtitle = entry
            .subtitle
            .take()
            .map(|s| sanitize_text(&s, 240))
            .filter(|s| !s.is_empty());
        let len = serde_json::to_vec(&entry).map(|v| v.len()).unwrap_or(1024);
        if bytes + len > 40 * 1024 || out.len() >= 60 {
            break;
        }
        bytes += len + 1;
        out.push(entry);
    }
    *entries = out;
}

fn extract_thumb(item: &Value) -> Option<RemoteResource> {
    let thumbs = item.get("thumbs")?;
    let raw = thumbs
        .get("url3")
        .or_else(|| thumbs.get("url2"))
        .or_else(|| thumbs.get("url1"))
        .or_else(|| thumbs.get("icon"))
        .and_then(Value::as_str)?
        .trim();
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
    Some(RemoteResource {
        url: normalized,
        headers: vec![
            ("User-Agent".into(), WEB_USER_AGENT.into()),
            ("Referer".into(), PAN_REFERER.into()),
        ],
        origin_grant: None,
    })
}

fn sort_filters(order: &str, desc: bool) -> Vec<SourceFilter> {
    vec![
        SourceFilter {
            id: "order".into(),
            label: "排序".into(),
            selected: Some(order.into()),
            options: vec![
                opt("name", "按名称"),
                opt("time", "按修改时间"),
                opt("size", "按文件大小"),
            ],
        },
        SourceFilter {
            id: "desc".into(),
            label: "顺序".into(),
            selected: Some(if desc { "1" } else { "0" }.into()),
            options: vec![opt("0", "升序"), opt("1", "降序")],
        },
    ]
}

fn opt(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.into(),
        label: label.into(),
    }
}
