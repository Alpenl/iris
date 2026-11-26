//! Response parsers for Bilibili catalog, search, dynamic, favorites, bangumi, history, and live.

use super::{
    BrowseKind,
    cards::{
        bangumi_filter, cover_resource, opt, parse_search_item, parse_season_folder,
        parse_video_card, strip_em,
    },
};
use crate::auth::sanitize_text;
use iris_plugin_sdk::api::{
    SourceEntryKind,
    v2::{
        Id,
        source::{SourceEntry, SourceFilter, SourcePage},
    },
};
use serde_json::Value;
use std::collections::BTreeSet;

pub fn parse_browse_response(
    source_id: &str,
    kind: &BrowseKind,
    json: &Value,
    authenticated: bool,
) -> Result<SourcePage, String> {
    let code = json.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == -101 {
        return Ok(SourcePage {
            source_id: source_id.into(),
            folder_id: None,
            parent_id: None,
            title: "需要登录哔哩哔哩".into(),
            entries: Vec::new(),
            next_page: None,
            authenticated: false,
            auth_hint: Some("当前列表需要登录账号，请点击「登录」扫码或填入 SESSDATA".into()),
            filters: Vec::new(),
        });
    }
    if code != 0 {
        let msg = json
            .get("message")
            .or_else(|| json.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("请求失败");
        return Err(format!("B站接口返回错误 ({code})：{msg}"));
    }
    let data = json
        .get("data")
        .or_else(|| json.get("result"))
        .unwrap_or(&Value::Null);

    let mut entries = Vec::new();
    let mut next_page = None;
    let mut folder_id = None;
    let mut parent_id = None;
    let mut filters = Vec::new();
    let title;

    match kind {
        BrowseKind::Recommend { page_num } => {
            title = "B站 · 首页推荐".into();
            if let Some(items) = data.get("item").and_then(Value::as_array) {
                for item in items {
                    if let Some(entry) = parse_video_card(item) {
                        entries.push(entry);
                    }
                }
            }
            if !entries.is_empty() && *page_num < 50 {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::Search {
            search_type,
            order,
            page_num,
            keyword,
        } => {
            title = format!("搜索：{keyword}");
            filters.push(SourceFilter {
                id: "search_type".into(),
                label: "类型".into(),
                selected: Some(search_type.clone()),
                options: vec![
                    opt("video", "视频"),
                    opt("media_bangumi", "番剧"),
                    opt("media_ft", "影视"),
                    opt("live_room", "直播"),
                ],
            });
            filters.push(SourceFilter {
                id: "order".into(),
                label: "排序".into(),
                selected: Some(order.clone()),
                options: vec![
                    opt("totalrank", "综合"),
                    opt("click", "最多播放"),
                    opt("pubdate", "最新发布"),
                    opt("dm", "最多弹幕"),
                ],
            });
            if let Some(items) = data.get("result").and_then(Value::as_array) {
                for item in items {
                    if let Some(entry) = parse_search_item(search_type, item) {
                        entries.push(entry);
                    }
                }
            }
            let num_pages = data.get("numPages").and_then(Value::as_u64).unwrap_or(1) as u32;
            if *page_num < num_pages && !entries.is_empty() {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::Popular { tab, page_num } => {
            title = "B站 · 热门与排行".into();
            filters.push(SourceFilter {
                id: "tab".into(),
                label: "榜单".into(),
                selected: Some(tab.clone()),
                options: vec![
                    opt("popular", "综合热门"),
                    opt("precious", "入站必刷"),
                    opt("rank_0", "全站排行"),
                    opt("rank_1", "动画排行"),
                    opt("rank_3", "音乐排行"),
                    opt("rank_4", "游戏排行"),
                    opt("rank_188", "科技数码"),
                    opt("rank_181", "影视排行"),
                ],
            });
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for item in list {
                    if let Some(entry) = parse_video_card(item) {
                        entries.push(entry);
                    }
                }
            }
            if tab == "popular"
                && data.get("no_more").and_then(Value::as_bool) != Some(true)
                && !entries.is_empty()
            {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::Dynamic => {
            title = "B站 · 关注动态".into();
            if let Some(items) = data.get("items").and_then(Value::as_array) {
                for item in items {
                    let author = item
                        .pointer("/modules/module_author/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(archive) = item.pointer("/modules/module_dynamic/major/archive")
                        && let Some(mut entry) = parse_video_card(archive)
                    {
                        if !author.is_empty() {
                            entry.subtitle = Some(match entry.subtitle {
                                Some(sub) => format!("{author} · {sub}"),
                                None => author.to_owned(),
                            });
                        }
                        entries.push(entry);
                    }
                }
            }
            if data.get("has_more").and_then(Value::as_bool) == Some(true) {
                next_page = data
                    .get("offset")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
        }
        BrowseKind::FavFolders { query } => {
            let q_lower = query.as_deref().map(str::to_lowercase);
            title = match query.as_deref() {
                Some(q) => format!("B站 · 我的收藏夹（搜索：{q}）"),
                None => "B站 · 我的收藏夹".into(),
            };
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for f in list {
                    let Some(id) = f.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let name = f.get("title").and_then(Value::as_str).unwrap_or("收藏夹");
                    if let Some(q) = q_lower.as_deref()
                        && !name.to_lowercase().contains(q)
                    {
                        continue;
                    }
                    let count = f.get("media_count").and_then(Value::as_u64).unwrap_or(0);
                    entries.push(SourceEntry {
                        id: format!("fav:{id}"),
                        name: name.to_owned(),
                        kind: SourceEntryKind::Folder,
                        size: None,
                        modified_secs: None,
                        duration: None,
                        subtitle: Some(format!("{count} 个内容")),
                        variants: Vec::new(),
                        thumbnail: None,
                    });
                }
            }
        }
        BrowseKind::FavItems {
            folder_id: fid,
            page_num,
            ..
        } => {
            folder_id = Some(fid.clone());
            parent_id = Some(String::new());
            title = data
                .pointer("/info/title")
                .and_then(Value::as_str)
                .unwrap_or("收藏夹")
                .to_owned();
            if let Some(medias) = data.get("medias").and_then(Value::as_array) {
                for m in medias {
                    if m.get("attr").and_then(Value::as_u64).unwrap_or(0) != 0 {
                        continue;
                    }
                    let bvid = m.get("bvid").and_then(Value::as_str).unwrap_or("");
                    if bvid.is_empty() {
                        continue;
                    }
                    let name = strip_em(m.get("title").and_then(Value::as_str).unwrap_or(bvid));
                    let upper = m
                        .pointer("/upper/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let page_cnt = m.get("page").and_then(Value::as_u64).unwrap_or(1);
                    entries.push(SourceEntry {
                        id: if page_cnt > 1 {
                            format!("ugc:{bvid}")
                        } else {
                            bvid.to_owned()
                        },
                        name,
                        kind: if page_cnt > 1 {
                            SourceEntryKind::Folder
                        } else {
                            SourceEntryKind::Video
                        },
                        size: None,
                        modified_secs: m.get("pubtime").and_then(Value::as_u64).map(Id),
                        duration: m.get("duration").and_then(Value::as_f64),
                        subtitle: Some(if page_cnt > 1 {
                            format!("{upper} · {page_cnt}P 合集")
                        } else {
                            upper.to_owned()
                        }),
                        variants: Vec::new(),
                        thumbnail: cover_resource(m.get("cover").and_then(Value::as_str)),
                    });
                }
            }
            if data.get("has_more").and_then(Value::as_bool) == Some(true) && !entries.is_empty() {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::BangumiFollow {
            follow_type,
            page_num,
        } => {
            title = "B站 · 番剧与影视".into();
            filters.push(bangumi_filter(
                &format!("follow_{follow_type}"),
                authenticated,
            ));
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for s in list {
                    if let Some(entry) = parse_season_folder(s) {
                        entries.push(entry);
                    }
                }
            }
            let total = data.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
            if page_num * 20 < total && !entries.is_empty() {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::BangumiIndex {
            season_type,
            page_num,
        } => {
            title = "B站 · 番剧与影视".into();
            filters.push(bangumi_filter(
                &format!("index_{season_type}"),
                authenticated,
            ));
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for s in list {
                    if let Some(entry) = parse_season_folder(s) {
                        entries.push(entry);
                    }
                }
            }
            if data.get("has_next").and_then(Value::as_u64) == Some(1) && !entries.is_empty() {
                next_page = Some((page_num + 1).to_string());
            }
        }
        BrowseKind::SeasonEpisodes { folder_id: fid } => {
            folder_id = Some(fid.clone());
            parent_id = Some(String::new());
            title = data
                .get("season_title")
                .or_else(|| data.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("剧集列表")
                .to_owned();
            if let Some(eps) = data.get("episodes").and_then(Value::as_array) {
                for ep in eps {
                    let Some(ep_id) = ep
                        .get("ep_id")
                        .or_else(|| ep.get("id"))
                        .and_then(Value::as_u64)
                    else {
                        continue;
                    };
                    let idx_title = ep.get("title").and_then(Value::as_str).unwrap_or("");
                    let long_title = ep.get("long_title").and_then(Value::as_str).unwrap_or("");
                    let badge = ep.get("badge").and_then(Value::as_str).unwrap_or("");
                    let name = if long_title.is_empty() {
                        format!("第 {idx_title} 话")
                    } else {
                        format!("{idx_title} · {long_title}")
                    };
                    entries.push(SourceEntry {
                        id: format!("ep:{ep_id}"),
                        name,
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: ep.get("pub_time").and_then(Value::as_u64).map(Id),
                        duration: ep.get("duration").and_then(Value::as_u64).map(|ms| {
                            if ms > 10_000 {
                                ms as f64 / 1000.
                            } else {
                                ms as f64
                            }
                        }),
                        subtitle: (!badge.is_empty()).then(|| badge.to_owned()),
                        variants: Vec::new(),
                        thumbnail: cover_resource(ep.get("cover").and_then(Value::as_str)),
                    });
                }
            }
        }
        BrowseKind::UgcPages {
            folder_id: fid,
            bvid,
        } => {
            folder_id = Some(fid.clone());
            parent_id = Some(String::new());
            title = data
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(bvid)
                .to_owned();
            let pic = data.get("pic").and_then(Value::as_str);
            if let Some(pages) = data.get("pages").and_then(Value::as_array) {
                for p in pages {
                    let Some(cid) = p.get("cid").and_then(Value::as_u64) else {
                        continue;
                    };
                    let page_no = p.get("page").and_then(Value::as_u64).unwrap_or(1);
                    let part = p.get("part").and_then(Value::as_str).unwrap_or("");
                    entries.push(SourceEntry {
                        id: format!("ugc:{bvid}:{cid}"),
                        name: if part.is_empty() {
                            format!("P{page_no}")
                        } else {
                            format!("P{page_no} · {part}")
                        },
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: None,
                        duration: p.get("duration").and_then(Value::as_f64),
                        subtitle: Some(bvid.clone()),
                        variants: Vec::new(),
                        thumbnail: cover_resource(
                            p.get("first_frame").and_then(Value::as_str).or(pic),
                        ),
                    });
                }
            }
        }
        BrowseKind::History => {
            title = "B站 · 观看历史".into();
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for item in list {
                    let hist = item.get("history").unwrap_or(&Value::Null);
                    let business = hist.get("business").and_then(Value::as_str).unwrap_or("");
                    let entry_id = match business {
                        "pgc" => hist
                            .get("epid")
                            .and_then(Value::as_u64)
                            .filter(|&v| v > 0)
                            .map(|ep| format!("ep:{ep}")),
                        "live" => hist
                            .get("oid")
                            .and_then(Value::as_u64)
                            .filter(|&v| v > 0)
                            .map(|r| format!("live:{r}")),
                        _ => hist
                            .get("bvid")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    };
                    let Some(id) = entry_id else { continue };
                    let name = item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned();
                    let author = item
                        .get("author_name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    entries.push(SourceEntry {
                        id,
                        name,
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: item.get("view_at").and_then(Value::as_u64).map(Id),
                        duration: item.get("duration").and_then(Value::as_f64),
                        subtitle: (!author.is_empty()).then(|| author.to_owned()),
                        variants: Vec::new(),
                        thumbnail: cover_resource(
                            item.get("cover").and_then(Value::as_str).or_else(|| {
                                item.get("covers")
                                    .and_then(Value::as_array)
                                    .and_then(|a| a.first())
                                    .and_then(Value::as_str)
                            }),
                        ),
                    });
                }
            }
            if let Some(cur) = data.get("cursor") {
                let max = cur.get("max").and_then(Value::as_u64).unwrap_or(0);
                let view_at = cur.get("view_at").and_then(Value::as_u64).unwrap_or(0);
                if max > 0 && view_at > 0 && !entries.is_empty() {
                    next_page = Some(format!("{max}:{view_at}"));
                }
            }
        }
        BrowseKind::Live { tab, page_num } => {
            title = "B站 · 哔哩直播".into();
            let mut opts = vec![opt("hot", "热门直播")];
            if authenticated {
                opts.push(opt("following", "关注开播"));
            }
            filters.push(SourceFilter {
                id: "tab".into(),
                label: "频道".into(),
                selected: Some(tab.clone()),
                options: opts,
            });
            if let Some(list) = data.get("list").and_then(Value::as_array) {
                for room in list {
                    if tab == "following"
                        && room.get("live_status").and_then(Value::as_u64) != Some(1)
                    {
                        continue;
                    }
                    let Some(room_id) = room.get("roomid").and_then(Value::as_u64) else {
                        continue;
                    };
                    let room_title = room
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("直播间");
                    let uname = room.get("uname").and_then(Value::as_str).unwrap_or("");
                    let area = room
                        .get("area_name")
                        .or_else(|| room.get("area_v2_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("直播");
                    entries.push(SourceEntry {
                        id: format!("live:{room_id}"),
                        name: strip_em(room_title),
                        kind: SourceEntryKind::Video,
                        size: None,
                        modified_secs: None,
                        duration: None,
                        subtitle: Some(format!("{uname} · {area}")),
                        variants: Vec::new(),
                        thumbnail: cover_resource(
                            room.get("cover")
                                .or_else(|| room.get("system_cover"))
                                .or_else(|| room.get("room_cover"))
                                .and_then(Value::as_str),
                        ),
                    });
                }
            }
            if !entries.is_empty() && *page_num < 30 {
                next_page = Some((page_num + 1).to_string());
            }
        }
    }

    let title = sanitize_text(&title, 240);
    finalize_page_entries(&mut entries)?;
    Ok(SourcePage {
        source_id: source_id.into(),
        folder_id,
        parent_id,
        title: if title.is_empty() {
            source_id.into()
        } else {
            title
        },
        entries,
        next_page,
        authenticated: true,
        auth_hint: None,
        filters,
    })
}

fn finalize_page_entries(entries: &mut Vec<SourceEntry>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut total = 0usize;
    for mut entry in entries.drain(..) {
        if entry.id.is_empty() || entry.id.len() > 256 || !seen.insert(entry.id.clone()) {
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
        if total + len + 1 > 8 * 1024 * 1024 || out.len() >= 10_000 {
            return Err("目录大小超过缓存限制，请缩小查询范围".into());
        }
        total += len + 1;
        out.push(entry);
    }
    *entries = out;
    Ok(())
}
