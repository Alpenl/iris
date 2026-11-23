//! Synchronous WebDAV HTTP/XML protocol helpers (`PROPFIND` `Depth: 1`,
//! `207 Multi-Status` XML parsing, HTTP-date timestamp decoding, and subtitle matching).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::library::{self, Kind};
use crate::vfs::http_client::{request_bytes, sanitize_header_value, validate_stream_url};

const MAX_PROPFIND_XML_BYTES: usize = 16 * 1024 * 1024;
const PROPFIND_REQUEST_XML: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>
"#;

const fn build_segment_encode_set() -> AsciiSet {
    let extra = *b"\"#%/<>?[\\]^`{|}";
    let mut set = CONTROLS.add(b' ');
    let mut i = 0;
    while i < extra.len() {
        set = set.add(extra[i]);
        i += 1;
    }
    set
}
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &build_segment_encode_set();

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum WebDavAuth {
    #[default]
    None,
    Basic {
        username: String,
    },
    Bearer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WebDavConnection {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub base_path: String,
    #[serde(default)]
    pub auth: WebDavAuth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WebDavFolder {
    pub path: String,
    pub name: String,
    pub modified_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WebDavEntry {
    pub path: String,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WebDavSubtitle {
    pub path: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WebDavListResult {
    pub path: String,
    pub parent_path: Option<String>,
    pub subfolders: Vec<WebDavFolder>,
    pub entries: Vec<WebDavEntry>,
    pub subtitles: Vec<WebDavSubtitle>,
}

pub(super) fn matching_subtitles<'a>(
    subtitles: &'a [WebDavSubtitle],
    video_name: &str,
) -> Vec<&'a WebDavSubtitle> {
    let Some(stem) = Path::new(video_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let stem_low = stem.to_ascii_lowercase();
    subtitles
        .iter()
        .filter(|sub| {
            let Some(s) = Path::new(&sub.name).file_stem().and_then(|s| s.to_str()) else {
                return false;
            };
            let low = s.to_ascii_lowercase();
            low == stem_low
                || (low.starts_with(&stem_low)
                    && matches!(
                        low.as_bytes().get(stem_low.len()),
                        Some(b'.' | b'_' | b'-' | b' ')
                    ))
        })
        .collect()
}

pub(super) fn normalize_dav_path(raw: &str) -> String {
    let segs = clean_decoded_segments(raw, false);
    if segs.is_empty() {
        "/".into()
    } else {
        format!("/{}", segs.join("/"))
    }
}

pub(super) fn parent_dav_path(normalized: &str) -> Option<String> {
    if normalized == "/" || normalized.is_empty() {
        return None;
    }
    let trimmed = normalized.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => Some("/".into()),
        Some(idx) => Some(trimmed[..idx].to_string()),
    }
}

fn clean_decoded_segments(raw_path: &str, percent_decode: bool) -> Vec<String> {
    let mut stack = Vec::new();
    for part in raw_path.split('/').filter(|p| !p.is_empty() && *p != ".") {
        let decoded = if percent_decode {
            percent_decode_str(part).decode_utf8_lossy().into_owned()
        } else {
            part.to_string()
        };
        match decoded.as_str() {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            _ => stack.push(decoded),
        }
    }
    stack
}

impl WebDavConnection {
    pub(super) fn auth_headers_with_secret(
        &self,
        secret: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        match &self.auth {
            WebDavAuth::None => {}
            WebDavAuth::Basic { username } => {
                let u = username.replace(['\r', '\n', '\0'], "");
                let p = secret.unwrap_or("").replace(['\r', '\n', '\0'], "");
                headers.insert(
                    "Authorization".into(),
                    format!("Basic {}", BASE64_STANDARD.encode(format!("{u}:{p}"))),
                );
            }
            WebDavAuth::Bearer => {
                if let Some(tok) = secret.map(sanitize_header_value).filter(|t| !t.is_empty()) {
                    headers.insert("Authorization".into(), format!("Bearer {tok}"));
                }
            }
        }
        headers
    }

    fn root_segments(&self) -> Result<(Url, Vec<String>)> {
        let mut base_url = validate_stream_url(&self.endpoint)?;
        base_url.set_query(None);
        base_url.set_fragment(None);
        let mut segs = clean_decoded_segments(base_url.path(), true);
        segs.extend(clean_decoded_segments(&self.base_path, false));
        Ok((base_url, segs))
    }

    pub(super) fn build_url(&self, rel_path: &str, is_dir: bool) -> Result<Url> {
        let (mut url, mut segs) = self.root_segments()?;
        segs.extend(clean_decoded_segments(rel_path, false));
        let joined = segs
            .iter()
            .map(|s| utf8_percent_encode(s, PATH_SEGMENT_ENCODE_SET).to_string())
            .collect::<Vec<_>>()
            .join("/");
        let path = if is_dir && !joined.is_empty() {
            format!("/{joined}/")
        } else {
            format!("/{joined}")
        };
        url.set_path(&path);
        Ok(url)
    }

    pub(super) fn list_dir(
        &self,
        secret: Option<&str>,
        rel_path: &str,
    ) -> Result<WebDavListResult> {
        let normalized = normalize_dav_path(rel_path);
        let (_, root_segs) = self.root_segments()?;
        let url = self.build_url(&normalized, true)?;

        let mut headers = self.auth_headers_with_secret(secret);
        headers.insert("Depth".into(), "1".into());
        headers.insert(
            "Content-Type".into(),
            "application/xml; charset=utf-8".into(),
        );

        let resp = request_bytes(
            "PROPFIND",
            url.as_str(),
            &headers,
            Some(PROPFIND_REQUEST_XML),
            MAX_PROPFIND_XML_BYTES,
        )
        .with_context(|| format!("WebDAV PROPFIND failed for {}", self.name))?;

        match resp.status {
            200 | 207 => {}
            401 | 403 => bail!("WebDAV authentication failed (HTTP {})", resp.status),
            404 => bail!("WebDAV folder not found (HTTP 404): {normalized}"),
            code => bail!("WebDAV server returned HTTP {code}"),
        }
        let xml = std::str::from_utf8(&resp.body).context("WebDAV response is not UTF-8")?;
        parse_propfind_xml(xml, &root_segs, &normalized)
    }
}

fn parse_month(m: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let p = m.get(..3)?.to_ascii_lowercase();
    MONTHS.iter().position(|&k| k == p).map(|i| i as u32 + 1)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    Some(era * 146_097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719_468)
}

/// Parses an HTTP-date (`<d:getlastmodified>`, RFC 1123 / RFC 850 / asctime) into Unix seconds.
pub(super) fn parse_http_date(raw: &str) -> Option<u64> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let (year, month, day, hms) = match parts.as_slice() {
        [_, d, mon, y, t, tz] if matches!(*tz, "GMT" | "UTC" | "+0000" | "-0000") => {
            (y.parse().ok()?, parse_month(mon)?, d.parse().ok()?, *t)
        }
        [_, dmy, t, tz] if matches!(*tz, "GMT" | "UTC") => {
            let mut sp = dmy.split('-');
            let (d, mon) = (sp.next()?.parse().ok()?, parse_month(sp.next()?)?);
            let yy: i64 = sp.next()?.parse().ok()?;
            let y = match yy {
                0..=69 => 2000 + yy,
                70..=99 => 1900 + yy,
                _ => yy,
            };
            (y, mon, d, *t)
        }
        [_, mon, d, t, y] => (y.parse().ok()?, parse_month(mon)?, d.parse().ok()?, *t),
        _ => return None,
    };
    let days = days_from_civil(year, month, day).filter(|&d| d >= 0)? as u64;
    let mut it = hms.split(':');
    let (h, m, s): (u64, u64, u64) = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    (it.next().is_none() && h < 24 && m < 60 && s <= 60)
        .then(|| days * 86_400 + h * 3_600 + m * 60 + s.min(59))
}

fn is_tag(node: roxmltree::Node<'_, '_>, local: &str) -> bool {
    node.is_element() && node.tag_name().name().eq_ignore_ascii_case(local)
}

fn child_by_tag<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    local: &str,
) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|c| is_tag(*c, local))
}

fn status_is_ok(status_text: &str) -> bool {
    status_text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .is_some_and(|c| (200..300).contains(&c))
}

pub(super) fn parse_propfind_xml(
    xml: &str,
    root_segments: &[String],
    queried_rel_path: &str,
) -> Result<WebDavListResult> {
    let normalized_dir = normalize_dav_path(queried_rel_path);
    let mut queried_full = root_segments.to_vec();
    queried_full.extend(clean_decoded_segments(&normalized_dir, false));

    let doc = roxmltree::Document::parse(xml).context("malformed WebDAV Multi-Status XML")?;
    let (mut subfolders, mut entries, mut subtitles) = (Vec::new(), Vec::new(), Vec::new());

    for resp_node in doc.descendants().filter(|n| is_tag(*n, "response")) {
        let Some(href_raw) = child_by_tag(resp_node, "href")
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let (mut ok_props, mut saw_propstat) = (Vec::new(), false);
        for ps in resp_node.children().filter(|n| is_tag(*n, "propstat")) {
            saw_propstat = true;
            if child_by_tag(ps, "status")
                .and_then(|s| s.text())
                .is_none_or(status_is_ok)
                && let Some(prop) = child_by_tag(ps, "prop")
            {
                ok_props.push(prop);
            }
        }
        if !saw_propstat && let Some(prop) = child_by_tag(resp_node, "prop") {
            ok_props.push(prop);
        }
        if saw_propstat && ok_props.is_empty() {
            continue;
        }

        let (mut is_collection, mut content_length, mut modified_secs, mut display_name) =
            (false, 0u64, 0u64, None);
        for prop in ok_props {
            if child_by_tag(prop, "resourcetype")
                .is_some_and(|rt| rt.descendants().any(|d| is_tag(d, "collection")))
            {
                is_collection = true;
            }
            if let Some(t) = child_by_tag(prop, "getcontentlength").and_then(|n| n.text())
                && let Ok(parsed) = t.trim().parse::<u64>()
            {
                content_length = parsed;
            }
            if let Some(t) = child_by_tag(prop, "getlastmodified").and_then(|n| n.text())
                && let Some(ts) = parse_http_date(t)
            {
                modified_secs = ts;
            }
            if let Some(dn) = child_by_tag(prop, "displayname")
                .and_then(|n| n.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                display_name = Some(dn.to_string());
            }
        }

        let is_abs_url = href_raw
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            || href_raw
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("https://"));
        let raw_path = if is_abs_url {
            let Ok(u) = Url::parse(href_raw) else {
                continue;
            };
            u.path().to_string()
        } else {
            href_raw
                .split(['?', '#'])
                .next()
                .unwrap_or(href_raw)
                .to_string()
        };
        is_collection |= raw_path.len() > 1 && raw_path.ends_with('/');
        let href_segs = clean_decoded_segments(&raw_path, true);
        let child_name = if raw_path.starts_with('/') {
            if href_segs.len() != queried_full.len() + 1 || !href_segs.starts_with(&queried_full) {
                continue;
            }
            href_segs[queried_full.len()].clone()
        } else if href_segs.len() == 1 && href_segs != queried_full {
            href_segs[0].clone()
        } else {
            continue;
        };
        if child_name.is_empty() || child_name.starts_with('.') {
            continue;
        }
        let item_rel = if normalized_dir == "/" {
            format!("/{child_name}")
        } else {
            format!("{normalized_dir}/{child_name}")
        };
        if is_collection {
            if !subfolders.iter().any(|f: &WebDavFolder| f.path == item_rel) {
                subfolders.push(WebDavFolder {
                    path: item_rel,
                    name: child_name,
                    modified_secs,
                });
            }
            continue;
        }

        let known = |s: &str| {
            library::kind_of(Path::new(s)).is_some() || library::is_subtitle(Path::new(s))
        };
        let Some(classify_name) = known(&child_name).then_some(child_name).or_else(|| {
            display_name
                .map(|s| s.replace(['/', '\\', '\0', '\r', '\n'], ""))
                .filter(|dn| !dn.is_empty() && known(dn))
        }) else {
            continue;
        };

        if let Some(kind) = library::kind_of(Path::new(&classify_name)) {
            if !entries.iter().any(|e: &WebDavEntry| e.path == item_rel) {
                entries.push(WebDavEntry {
                    path: item_rel,
                    name: classify_name,
                    kind,
                    size: content_length,
                    modified_secs,
                });
            }
        } else if library::is_subtitle(Path::new(&classify_name))
            && !subtitles
                .iter()
                .any(|s: &WebDavSubtitle| s.path == item_rel)
        {
            subtitles.push(WebDavSubtitle {
                path: item_rel,
                name: classify_name,
            });
        }
    }

    subfolders.sort_by(|a, b| library::natural_cmp(&a.name, &b.name));
    entries.sort_by(|a, b| library::natural_cmp(&a.name, &b.name));
    subtitles.sort_by(|a, b| library::natural_cmp(&a.name, &b.name));
    Ok(WebDavListResult {
        path: normalized_dir.clone(),
        parent_path: parent_dav_path(&normalized_dir),
        subfolders,
        entries,
        subtitles,
    })
}
