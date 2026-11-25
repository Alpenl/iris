//! BiliSail-aligned catalog endpoints:
//! Recommend (`top/feed/rcmd`), Popular & Ranking (`popular` / `precious` / `ranking/v2`),
//! WBI Search (`wbi/search/type`), Dynamic (`web-dynamic`), Favorites (`x/v3/fav`),
//! Bangumi/PGC (`bangumi/follow/list`, `pgc/season/index/result`, `pgc/view/web/season`),
//! Cloud History (`history/cursor`), Live (`xlive`), and multi-P UGC folders.

mod parser;

pub use parser::{cover_resource, parse_browse_response, strip_em};

use crate::{
    auth::{StoredSession, WbiKeys},
    wbi,
};
use std::collections::{BTreeMap, BTreeSet};

pub struct BrowsePlan {
    pub url: String,
    pub needs_wbi: bool,
    pub kind: BrowseKind,
}

#[derive(Clone)]
pub enum BrowseKind {
    Recommend {
        page_num: u32,
    },
    Search {
        search_type: String,
        order: String,
        page_num: u32,
        keyword: String,
    },
    Popular {
        tab: String,
        page_num: u32,
    },
    Dynamic,
    FavFolders,
    FavItems {
        folder_id: String,
        media_id: String,
        page_num: u32,
    },
    BangumiFollow {
        follow_type: String,
        page_num: u32,
    },
    BangumiIndex {
        season_type: String,
        page_num: u32,
    },
    SeasonEpisodes {
        folder_id: String,
    },
    UgcPages {
        folder_id: String,
        bvid: String,
    },
    History,
    Live {
        tab: String,
        page_num: u32,
    },
}

#[allow(clippy::too_many_arguments)]
pub fn plan_browse(
    source_id: &str,
    folder_id: Option<&str>,
    cursor: Option<&str>,
    query: Option<&str>,
    filters: &BTreeMap<String, String>,
    session: &StoredSession,
    wbi: &WbiKeys,
    now_secs: u64,
) -> Result<BrowsePlan, String> {
    if let Some(folder) = folder_id {
        if let Some(media_id) = folder.strip_prefix("fav:") {
            let page_num = cursor
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1)
                .max(1);
            let mut params = vec![
                ("media_id", media_id.to_owned()),
                ("pn", page_num.to_string()),
                ("ps", "20".into()),
                ("platform", "web".into()),
            ];
            if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
                params.push(("keyword", q.to_owned()));
            }
            return Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/v3/fav/resource/list?{}",
                    wbi::unsigned_query(&params)
                ),
                needs_wbi: false,
                kind: BrowseKind::FavItems {
                    folder_id: folder.to_owned(),
                    media_id: media_id.to_owned(),
                    page_num,
                },
            });
        }
        if let Some(season_id) = folder.strip_prefix("ss:") {
            return Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/pgc/view/web/season?season_id={}",
                    wbi::encode_query_component(season_id)
                ),
                needs_wbi: false,
                kind: BrowseKind::SeasonEpisodes {
                    folder_id: folder.to_owned(),
                },
            });
        }
        if let Some(bvid) = folder.strip_prefix("ugc:") {
            return Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/web-interface/view?bvid={}",
                    wbi::encode_query_component(bvid)
                ),
                needs_wbi: false,
                kind: BrowseKind::UgcPages {
                    folder_id: folder.to_owned(),
                    bvid: bvid.to_owned(),
                },
            });
        }
    }

    if let Some(keyword) = query.map(str::trim).filter(|s| !s.is_empty())
        && matches!(source_id, "recommend" | "popular" | "bangumi" | "live")
    {
        let default_type = match source_id {
            "bangumi" => "media_bangumi",
            "live" => "live_room",
            _ => "video",
        };
        let search_type = filters
            .get("search_type")
            .map(String::as_str)
            .unwrap_or(default_type)
            .to_owned();
        let order = filters
            .get("order")
            .map(String::as_str)
            .unwrap_or("totalrank")
            .to_owned();
        let page_num = cursor
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);
        let params = vec![
            ("search_type", search_type.clone()),
            ("keyword", keyword.to_owned()),
            ("order", order.clone()),
            ("page", page_num.to_string()),
            ("page_size", "20".into()),
        ];
        let query_str = if let Some(mixin) = wbi.mixin() {
            wbi::sign_wbi_query(&params, &mixin, now_secs)
        } else {
            wbi::unsigned_query(&params)
        };
        return Ok(BrowsePlan {
            url: format!("https://api.bilibili.com/x/web-interface/wbi/search/type?{query_str}"),
            needs_wbi: wbi.mixin().is_none(),
            kind: BrowseKind::Search {
                search_type,
                order,
                page_num,
                keyword: keyword.to_owned(),
            },
        });
    }

    let page_num = cursor
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .max(1);
    match source_id {
        "recommend" => {
            let params = vec![
                ("ps", "20".into()),
                ("fresh_idx", page_num.to_string()),
                ("fresh_idx_1h", page_num.to_string()),
                ("fresh_type", "4".into()),
                ("feed_version", "V8".into()),
            ];
            let query_str = if let Some(mixin) = wbi.mixin() {
                wbi::sign_wbi_query(&params, &mixin, now_secs)
            } else {
                wbi::unsigned_query(&params)
            };
            Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/web-interface/wbi/index/top/feed/rcmd?{query_str}"
                ),
                needs_wbi: wbi.mixin().is_none(),
                kind: BrowseKind::Recommend { page_num },
            })
        }
        "popular" => {
            let tab = filters
                .get("tab")
                .map(String::as_str)
                .unwrap_or("popular")
                .to_owned();
            let url = match tab.as_str() {
                "precious" => {
                    "https://api.bilibili.com/x/web-interface/popular/precious?page_size=50&page=1"
                        .into()
                }
                t if t.starts_with("rank_") => {
                    let rid = t.strip_prefix("rank_").unwrap_or("0");
                    format!(
                        "https://api.bilibili.com/x/web-interface/ranking/v2?rid={rid}&type=all"
                    )
                }
                _ => {
                    format!("https://api.bilibili.com/x/web-interface/popular?ps=20&pn={page_num}")
                }
            };
            Ok(BrowsePlan {
                url,
                needs_wbi: false,
                kind: BrowseKind::Popular { tab, page_num },
            })
        }
        "dynamic" => {
            if !session.is_authenticated() {
                return Err("请先扫码或使用 Cookie 登录哔哩哔哩账号".into());
            }
            let mut params = vec![("type", "video".into())];
            if let Some(off) = cursor.filter(|s| !s.is_empty()) {
                params.push(("offset", off.to_owned()));
            }
            Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/all?{}",
                    wbi::unsigned_query(&params)
                ),
                needs_wbi: false,
                kind: BrowseKind::Dynamic,
            })
        }
        "favorites" => {
            if !session.is_authenticated() || session.dede_user_id.is_empty() {
                return Err("请先登录哔哩哔哩账号以读取云端收藏夹".into());
            }
            Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/v3/fav/folder/created/list-all?up_mid={}",
                    wbi::encode_query_component(&session.dede_user_id)
                ),
                needs_wbi: false,
                kind: BrowseKind::FavFolders,
            })
        }
        "bangumi" => {
            let default_tab = if session.is_authenticated() && !session.dede_user_id.is_empty() {
                "follow_1"
            } else {
                "index_1"
            };
            let tab = filters
                .get("tab")
                .map(String::as_str)
                .unwrap_or(default_tab);
            if let Some(ft) = tab.strip_prefix("follow_")
                && session.is_authenticated()
                && !session.dede_user_id.is_empty()
            {
                Ok(BrowsePlan {
                    url: format!(
                        "https://api.bilibili.com/x/space/bangumi/follow/list?type={ft}&follow_status=0&pn={page_num}&ps=20&vmid={}",
                        wbi::encode_query_component(&session.dede_user_id)
                    ),
                    needs_wbi: false,
                    kind: BrowseKind::BangumiFollow {
                        follow_type: ft.to_owned(),
                        page_num,
                    },
                })
            } else {
                let st = tab.strip_prefix("index_").unwrap_or("1");
                Ok(BrowsePlan {
                    url: format!(
                        "https://api.bilibili.com/pgc/season/index/result?season_type={st}&type=1&order=2&page={page_num}&pagesize=20"
                    ),
                    needs_wbi: false,
                    kind: BrowseKind::BangumiIndex {
                        season_type: st.to_owned(),
                        page_num,
                    },
                })
            }
        }
        "history" => {
            if !session.is_authenticated() {
                return Err("请先登录哔哩哔哩账号以读取观看历史".into());
            }
            let (max, view_at) = cursor.and_then(|c| c.split_once(':')).unwrap_or(("0", "0"));
            Ok(BrowsePlan {
                url: format!(
                    "https://api.bilibili.com/x/web-interface/history/cursor?ps=20&max={}&view_at={}&type=all",
                    wbi::encode_query_component(max),
                    wbi::encode_query_component(view_at)
                ),
                needs_wbi: false,
                kind: BrowseKind::History,
            })
        }
        "live" => {
            let tab = filters
                .get("tab")
                .map(String::as_str)
                .unwrap_or("hot")
                .to_owned();
            if tab == "following" && session.is_authenticated() {
                Ok(BrowsePlan {
                    url: format!(
                        "https://api.live.bilibili.com/xlive/web-ucenter/user/following?page={page_num}&page_size=20"
                    ),
                    needs_wbi: false,
                    kind: BrowseKind::Live { tab, page_num },
                })
            } else {
                Ok(BrowsePlan {
                    url: format!(
                        "https://api.live.bilibili.com/xlive/web-interface/v1/second/getListByArea?sort=online&page={page_num}&page_size=20"
                    ),
                    needs_wbi: false,
                    kind: BrowseKind::Live {
                        tab: "hot".into(),
                        page_num,
                    },
                })
            }
        }
        _ => Err("未知的哔哩哔哩数据源".into()),
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
    page: &mut iris_plugin_sdk::api::v2::source::SourcePage,
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
