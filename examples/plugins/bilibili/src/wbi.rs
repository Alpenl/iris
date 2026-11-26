//! Bilibili WBI parameter signing (RFC 1321 MD5 + 64-byte mixin table),
//! `buvid3` device token formatting, and BV/av/ep/ss/live URL target parsing.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use url::Url;

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

/// RFC 3986 unreserved characters (`A-Z a-z 0-9 - _ . ~`), uppercase hex escapes.
const WBI_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BiliTarget {
    Bvid {
        bvid: String,
        cid: Option<u64>,
        page: usize,
    },
    Aid {
        aid: u64,
        cid: Option<u64>,
        page: usize,
    },
    Episode(u64),
    Season(u64),
    LiveRoom(u64),
    ShortUrl(String),
}

pub fn md5_hex(input: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

pub fn extract_wbi_stem(url_str: &str) -> Option<String> {
    let name = url_str.rsplit('/').next()?.split('?').next()?;
    let (stem, _) = name.split_once('.').unwrap_or((name, ""));
    (!stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| stem.to_owned())
}

pub fn mixin_key(img_key: &str, sub_key: &str) -> Option<String> {
    let combined = format!("{img_key}{sub_key}");
    let bytes = combined.as_bytes();
    if bytes.len() < 64 {
        return None;
    }
    let mut out = String::with_capacity(32);
    for &idx in &MIXIN_KEY_ENC_TAB[..32] {
        out.push(*bytes.get(idx)? as char);
    }
    Some(out)
}

pub fn encode_query_component(raw: &str) -> String {
    utf8_percent_encode(raw, WBI_ENCODE_SET).to_string()
}

pub fn percent_decode_utf8(raw: &str) -> Option<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .ok()
        .map(|s| s.into_owned())
}

/// Parses `iris-plugin://community.bilibili/<source>/<entry>` (or `plugin://`),
/// UTF-8 percent-decoding `<source>` and `<entry>` to match `MediaUri::Plugin`.
pub fn parse_plugin_uri(uri: &str) -> Option<(String, String)> {
    let rest =
        strip_prefix_ci(uri, "iris-plugin://").or_else(|| strip_prefix_ci(uri, "plugin://"))?;
    let mut parts = rest.splitn(3, '/');
    let plugin_id = percent_decode_utf8(parts.next()?)?;
    if plugin_id != "community.bilibili" {
        return None;
    }
    let source_id = percent_decode_utf8(parts.next()?)?;
    let entry_id = percent_decode_utf8(parts.next()?)?;
    if source_id.is_empty() || entry_id.is_empty() {
        return None;
    }
    Some((source_id, entry_id))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

pub fn sign_wbi_query(params: &[(&str, String)], mixin: &str, wts_secs: u64) -> String {
    let mut sorted: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| {
            let cleaned: String = v
                .chars()
                .filter(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*'))
                .collect();
            ((*k).to_owned(), cleaned)
        })
        .collect();
    sorted.push(("wts".into(), wts_secs.to_string()));
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let query = sorted
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                encode_query_component(k),
                encode_query_component(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let w_rid = md5_hex(format!("{query}{mixin}").as_bytes());
    format!("{query}&w_rid={w_rid}")
}

pub fn unsigned_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                encode_query_component(k),
                encode_query_component(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub fn generate_buvid3(entropy: &[u8; 16], suffix_ms: u64) -> String {
    let hex: Vec<String> = entropy.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{:05}infoc",
        hex[0],
        hex[1],
        hex[2],
        hex[3],
        hex[4],
        hex[5],
        hex[6],
        hex[7],
        hex[8],
        hex[9],
        hex[10],
        hex[11],
        hex[12],
        hex[13],
        hex[14],
        suffix_ms % 100_000
    )
}

pub fn canonical_target_url(raw: &str) -> Option<String> {
    match parse_target(raw)? {
        BiliTarget::Bvid { bvid, page, .. } if page > 1 => {
            Some(format!("https://www.bilibili.com/video/{bvid}?p={page}"))
        }
        BiliTarget::Bvid { bvid, .. } => Some(format!("https://www.bilibili.com/video/{bvid}")),
        BiliTarget::Aid { aid, page, .. } if page > 1 => {
            Some(format!("https://www.bilibili.com/video/av{aid}?p={page}"))
        }
        BiliTarget::Aid { aid, .. } => Some(format!("https://www.bilibili.com/video/av{aid}")),
        BiliTarget::Episode(ep) => Some(format!("https://www.bilibili.com/bangumi/play/ep{ep}")),
        BiliTarget::Season(ss) => Some(format!("https://www.bilibili.com/bangumi/play/ss{ss}")),
        BiliTarget::LiveRoom(room) => Some(format!("https://live.bilibili.com/{room}")),
        BiliTarget::ShortUrl(short) => Some(short),
    }
}

pub fn parse_target(raw: &str) -> Option<BiliTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("ugc:") {
        let mut parts = rest.split(':');
        let id = parts.next()?;
        let cid = parts.next().and_then(|s| s.parse::<u64>().ok());
        if id.starts_with("BV") && valid_bvid(id) {
            return Some(BiliTarget::Bvid {
                bvid: id.to_owned(),
                cid,
                page: 1,
            });
        }
        if let Some(aid) = id.strip_prefix("av").and_then(|s| s.parse::<u64>().ok()) {
            return Some(BiliTarget::Aid { aid, cid, page: 1 });
        }
    }
    if let Some(ep) = trimmed
        .strip_prefix("ep:")
        .or_else(|| trimmed.strip_prefix("ep"))
        .or_else(|| trimmed.strip_prefix("EP"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(BiliTarget::Episode(ep));
    }
    if let Some(ss) = trimmed
        .strip_prefix("ss:")
        .or_else(|| trimmed.strip_prefix("ss"))
        .or_else(|| trimmed.strip_prefix("SS"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(BiliTarget::Season(ss));
    }
    if let Some(room) = trimmed
        .strip_prefix("live:")
        .or_else(|| trimmed.strip_prefix("room:"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(BiliTarget::LiveRoom(room));
    }
    if valid_bvid(trimmed) {
        return Some(BiliTarget::Bvid {
            bvid: normalize_bvid_prefix(trimmed),
            cid: None,
            page: 1,
        });
    }
    if let Some(aid) = trimmed
        .strip_prefix("av")
        .or_else(|| trimmed.strip_prefix("AV"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(BiliTarget::Aid {
            aid,
            cid: None,
            page: 1,
        });
    }
    if let Ok(url) = Url::parse(trimmed) {
        return parse_bilibili_url(&url);
    }
    None
}

pub fn parse_bilibili_url(url: &Url) -> Option<BiliTarget> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "b23.tv" || host.ends_with(".b23.tv") {
        let seg = url.path_segments()?.find(|s| !s.is_empty())?;
        if let Some(direct) = parse_target(seg) {
            return Some(direct);
        }
        return Some(BiliTarget::ShortUrl(url.to_string()));
    }
    let page = url
        .query_pairs()
        .find(|(k, _)| k == "p")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .filter(|&p| p >= 1)
        .unwrap_or(1);
    if host == "live.bilibili.com" {
        let room = url
            .path_segments()?
            .filter(|s| !s.is_empty())
            .find_map(|s| s.parse::<u64>().ok())?;
        return Some(BiliTarget::LiveRoom(room));
    }
    if host != "bilibili.com" && !host.ends_with(".bilibili.com") {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    for seg in &segs {
        if valid_bvid(seg) {
            return Some(BiliTarget::Bvid {
                bvid: normalize_bvid_prefix(seg),
                cid: None,
                page,
            });
        }
        if let Some(aid) = seg
            .strip_prefix("av")
            .or_else(|| seg.strip_prefix("AV"))
            .and_then(|s| s.parse::<u64>().ok())
        {
            return Some(BiliTarget::Aid {
                aid,
                cid: None,
                page,
            });
        }
        if let Some(ep) = seg
            .strip_prefix("ep")
            .or_else(|| seg.strip_prefix("EP"))
            .and_then(|s| s.parse::<u64>().ok())
        {
            return Some(BiliTarget::Episode(ep));
        }
        if let Some(ss) = seg
            .strip_prefix("ss")
            .or_else(|| seg.strip_prefix("SS"))
            .and_then(|s| s.parse::<u64>().ok())
        {
            return Some(BiliTarget::Season(ss));
        }
    }
    if let Some((_, bvid)) = url
        .query_pairs()
        .find(|(k, v)| k == "bvid" && valid_bvid(v))
    {
        return Some(BiliTarget::Bvid {
            bvid: normalize_bvid_prefix(&bvid),
            cid: None,
            page,
        });
    }
    None
}

fn valid_bvid(s: &str) -> bool {
    (s.starts_with("BV1") || s.starts_with("bv1"))
        && s.len() == 12
        && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn normalize_bvid_prefix(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("bv") {
        format!("BV{rest}")
    } else {
        s.to_owned()
    }
}
