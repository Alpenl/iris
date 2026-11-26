//! Baidu Netdisk catalog browsing (`files` directory hierarchy, `videos`/`photos`/`audio`
//! category views, and global recursive search), pagination, caching, and playlist building.

mod paging;
mod parser;

pub(crate) use paging::{PageCache, PageTarget};
pub use parser::{ParsedBrowse, parse_browse_response};

use crate::{
    auth::{AuthMode, BAIDU_APP_ID, Preferences, StoredSession},
    media::{self, MediaType},
    sign::{build_query, sanitize_text},
};
use iris_plugin_sdk::api::v2::{
    enhance::SubtitleFormat,
    source::{PlaylistEntry, SourcePage},
};
use std::collections::{BTreeMap, BTreeSet};

pub const PAGE_SIZE: u32 = 50;

#[derive(Clone, Debug)]
pub struct BrowsePlan {
    pub url: String,
    pub use_pcs_ua: bool,
    pub kind: BrowseKind,
}

#[derive(Clone, Debug)]
pub enum BrowseKind {
    DirList {
        dir: String,
        order: String,
        desc: bool,
        page_num: u32,
    },
    CategoryList {
        category: u8,
        order: String,
        desc: bool,
        page_num: u32,
    },
    Search {
        keyword: String,
        dir: String,
        page_num: u32,
        category_filter: Option<MediaType>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedSubtitle {
    pub fs_id: u64,
    pub path: String,
    pub name: String,
    pub format: SubtitleFormat,
    pub lang: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedMediaItem {
    pub fs_id: u64,
    pub path: String,
    pub name: String,
    pub kind: MediaType,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct FolderCache {
    pub media: Vec<CachedMediaItem>,
    pub subtitles: Vec<CachedSubtitle>,
}

/// Encodes a stable entry identifier carrying both `fs_id` and full `path` so
/// bookmarks and resume playback work even without an in-memory folder cache.
pub fn encode_entry_id(fs_id: u64, path: &str) -> String {
    format!("bd:{fs_id}:{path}")
}

/// Decodes `bd:{fs_id}:{path}` or a raw path `/...` into `(fs_id, path)`.
pub fn decode_entry_id(raw: &str) -> Option<(u64, String)> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("bd:") {
        let (id_str, path) = rest.split_once(':')?;
        let fs_id = id_str.parse::<u64>().ok()?;
        let norm = media::normalize_dir(path);
        if norm == "/" {
            return None;
        }
        return Some((fs_id, norm));
    }
    if trimmed.starts_with('/') && trimmed.len() > 1 {
        return Some((0, media::normalize_dir(trimmed)));
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn plan_browse(
    source_id: &str,
    folder_id: Option<&str>,
    cursor: Option<&str>,
    query: Option<&str>,
    filters: &BTreeMap<String, String>,
    session: &StoredSession,
    prefs: &Preferences,
    now_ms: u64,
) -> Result<BrowsePlan, String> {
    if !session.is_authenticated() {
        return Err("请先扫码、粘贴 Cookie 或配置开放平台令牌登录百度网盘".into());
    }
    let page_num = cursor
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    let order = match filters
        .get("order")
        .map(String::as_str)
        .unwrap_or(prefs.default_order.as_str())
    {
        "time" => "time",
        "size" => "size",
        _ => "name",
    }
    .to_owned();
    let default_desc = matches!(source_id, "videos" | "photos" | "audio") || order == "time";
    let desc = match filters.get("desc").map(String::as_str) {
        Some("1") => true,
        Some("0") => false,
        _ => default_desc,
    };

    if let Some(keyword) = query.map(str::trim).filter(|s| !s.is_empty()) {
        let dir = if source_id == "files" {
            folder_id
                .map(media::normalize_dir)
                .unwrap_or_else(|| "/".into())
        } else {
            "/".into()
        };
        let category_filter = match source_id {
            "videos" => Some(MediaType::Video),
            "photos" => Some(MediaType::Photo),
            "audio" => Some(MediaType::Audio),
            _ => None,
        };
        let (url, use_pcs_ua) = match session.mode {
            AuthMode::Cookie => {
                let mut params = vec![
                    ("clienttype", "0".into()),
                    ("app_id", BAIDU_APP_ID.into()),
                    ("web", "1".into()),
                    ("order", "time".into()),
                    ("desc", "1".into()),
                    ("num", PAGE_SIZE.to_string()),
                    ("page", page_num.to_string()),
                    ("recursion", "1".into()),
                    ("key", keyword.to_owned()),
                ];
                if dir != "/" {
                    params.push(("dir", dir.clone()));
                }
                (
                    format!("https://pan.baidu.com/api/search?{}", build_query(&params)),
                    false,
                )
            }
            AuthMode::OpenApi => {
                let params = vec![
                    ("method", "search".into()),
                    ("access_token", session.access_token.clone()),
                    ("key", keyword.to_owned()),
                    ("dir", dir.clone()),
                    ("recursion", "1".into()),
                    ("web", "1".into()),
                    ("page", page_num.to_string()),
                    ("num", PAGE_SIZE.to_string()),
                ];
                (
                    format!(
                        "https://pan.baidu.com/rest/2.0/xpan/file?{}",
                        build_query(&params)
                    ),
                    false,
                )
            }
        };
        return Ok(BrowsePlan {
            url,
            use_pcs_ua,
            kind: BrowseKind::Search {
                keyword: keyword.to_owned(),
                dir,
                page_num,
                category_filter,
            },
        });
    }

    match source_id {
        "files" => {
            let dir = folder_id
                .map(media::normalize_dir)
                .unwrap_or_else(|| "/".into());
            let (url, use_pcs_ua) = match session.mode {
                AuthMode::Cookie => {
                    let params = vec![
                        ("method", "list".into()),
                        ("order", order.clone()),
                        ("desc", if desc { "1" } else { "0" }.into()),
                        ("showempty", "0".into()),
                        ("web", "1".into()),
                        ("page", page_num.to_string()),
                        ("num", PAGE_SIZE.to_string()),
                        ("dir", dir.clone()),
                        ("t", now_ms.to_string()),
                    ];
                    (
                        format!(
                            "https://pan.baidu.com/rest/2.0/xpan/file?{}",
                            build_query(&params)
                        ),
                        true,
                    )
                }
                AuthMode::OpenApi => {
                    let start = (page_num - 1) * PAGE_SIZE;
                    let mut params = vec![
                        ("method", "list".into()),
                        ("access_token", session.access_token.clone()),
                        ("dir", dir.clone()),
                        ("web", "web".into()),
                        ("order", order.clone()),
                        ("start", start.to_string()),
                        ("limit", PAGE_SIZE.to_string()),
                    ];
                    if desc {
                        params.push(("desc", "1".into()));
                    }
                    (
                        format!(
                            "https://pan.baidu.com/rest/2.0/xpan/file?{}",
                            build_query(&params)
                        ),
                        false,
                    )
                }
            };
            Ok(BrowsePlan {
                url,
                use_pcs_ua,
                kind: BrowseKind::DirList {
                    dir,
                    order,
                    desc,
                    page_num,
                },
            })
        }
        "videos" | "photos" | "audio" => {
            let category = match source_id {
                "videos" => 1u8,
                "audio" => 2u8,
                _ => 3u8,
            };
            let (url, use_pcs_ua) = match session.mode {
                AuthMode::Cookie => {
                    let params = vec![
                        ("category", category.to_string()),
                        ("order", order.clone()),
                        ("desc", if desc { "1" } else { "0" }.into()),
                        ("page", page_num.to_string()),
                        ("num", PAGE_SIZE.to_string()),
                        ("show_all", "1".into()),
                        ("web", "1".into()),
                        ("clienttype", "0".into()),
                        ("app_id", BAIDU_APP_ID.into()),
                    ];
                    (
                        format!(
                            "https://pan.baidu.com/api/categorylist?{}",
                            build_query(&params)
                        ),
                        false,
                    )
                }
                AuthMode::OpenApi => {
                    let start = (page_num - 1) * PAGE_SIZE;
                    let mut params = vec![
                        ("method", "categorylist".into()),
                        ("access_token", session.access_token.clone()),
                        ("category", category.to_string()),
                        ("parent_path", "/".into()),
                        ("recursion", "1".into()),
                        ("order", order.clone()),
                        ("start", start.to_string()),
                        ("limit", PAGE_SIZE.to_string()),
                    ];
                    if desc {
                        params.push(("desc", "1".into()));
                    }
                    (
                        format!(
                            "https://pan.baidu.com/rest/2.0/xpan/file?{}",
                            build_query(&params)
                        ),
                        false,
                    )
                }
            };
            Ok(BrowsePlan {
                url,
                use_pcs_ua,
                kind: BrowseKind::CategoryList {
                    category,
                    order,
                    desc,
                    page_num,
                },
            })
        }
        _ => Err("未知的百度网盘数据源".into()),
    }
}

pub fn build_sibling_playlist(
    folder_cache: &FolderCache,
    source_id: &str,
    target_path: &str,
    target_kind: MediaType,
) -> Vec<PlaylistEntry> {
    let siblings: Vec<&CachedMediaItem> = folder_cache
        .media
        .iter()
        .filter(|m| m.kind == target_kind)
        .collect();
    if siblings.len() <= 1 {
        return Vec::new();
    }
    let idx = siblings
        .iter()
        .position(|m| m.path == target_path)
        .unwrap_or(0);
    let start = idx.saturating_sub(16);
    siblings
        .into_iter()
        .skip(start)
        .take(48)
        .map(|item| PlaylistEntry {
            source_id: source_id.into(),
            entry_id: encode_entry_id(item.fs_id, &item.path),
            title: sanitize_text(&item.name, 240),
        })
        .collect()
}

pub fn merge_folder_cache(
    caches: &mut BTreeMap<String, FolderCache>,
    dir: &str,
    is_first_page: bool,
    media_items: Vec<CachedMediaItem>,
    subtitles: Vec<CachedSubtitle>,
) {
    if caches.len() >= 8
        && !caches.contains_key(dir)
        && let Some(oldest) = caches.keys().next().cloned()
    {
        caches.remove(&oldest);
    }
    let entry = caches.entry(dir.to_owned()).or_default();
    if is_first_page {
        entry.media.clear();
        entry.subtitles.clear();
    }
    for m in media_items {
        if entry.media.len() < 400 && !entry.media.iter().any(|x| x.path == m.path) {
            entry.media.push(m);
        }
    }
    for s in subtitles {
        if entry.subtitles.len() < 120 && !entry.subtitles.iter().any(|x| x.path == s.path) {
            entry.subtitles.push(s);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dedup_browse_page(
    browse_seen: &mut BTreeMap<String, BTreeSet<String>>,
    browse_cursors: &mut BTreeMap<String, BTreeSet<String>>,
    source_id: &str,
    folder_id: Option<&str>,
    query: Option<&str>,
    filters: &BTreeMap<String, String>,
    cursor: Option<&str>,
    page: &mut SourcePage,
) {
    let key = format!(
        "{source_id}|{}|{}|{filters:?}",
        folder_id.unwrap_or(""),
        query.unwrap_or("")
    );
    if cursor.is_none() {
        if browse_seen.len() >= 4 && !browse_seen.contains_key(&key) {
            browse_seen.clear();
            browse_cursors.clear();
        }
        browse_seen.insert(key.clone(), BTreeSet::new());
        browse_cursors.insert(key.clone(), BTreeSet::new());
    }
    let seen = browse_seen.entry(key.clone()).or_default();
    let cursors = browse_cursors.entry(key).or_default();
    if let Some(c) = cursor {
        cursors.insert(c.to_owned());
    }
    page.entries.retain(|entry| {
        if seen.contains(&entry.id) {
            false
        } else {
            if seen.len() < 2000 {
                seen.insert(entry.id.clone());
            }
            true
        }
    });
    if page
        .next_page
        .as_ref()
        .is_some_and(|next| cursors.contains(next) || cursor == Some(next.as_str()))
        || (cursor.is_some() && page.entries.is_empty())
    {
        page.next_page = None;
    }
}
