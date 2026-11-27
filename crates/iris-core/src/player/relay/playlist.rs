//! Strict RFC 8216 subset parser and sealed-URI rewriter for authenticated HLS manifests.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use url::Url;

use iris_net::http_client::validate_stream_url;

use super::{RouteKind, Routes};

const MAX_REWRITTEN_PLAYLIST: usize = 16 * 1024 * 1024;

pub(super) fn is_hls(prefix: &[u8]) -> bool {
    prefix
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(prefix)
        .starts_with(b"#EXTM3U")
}

pub(super) fn rewrite_playlist(input: &str, base: &Url, routes: &Routes) -> Result<String> {
    rewrite_playlist_inner(input, base, routes, None)
}

fn rewrite_playlist_inner(
    input: &str,
    base: &Url,
    routes: &Routes,
    mut extracted: Option<&mut Vec<String>>,
) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let lines = input.trim_start_matches('\u{feff}');
    ensure!(
        !lines.contains('\0') && !lines.contains("{$"),
        "unsupported HLS playlist syntax"
    );
    ensure!(
        lines.lines().next() == Some("#EXTM3U"),
        "invalid HLS playlist"
    );
    let mut variant = false;
    let mut rewritten = BTreeMap::new();
    for line in lines.lines() {
        // Match the demuxer's treatment of surrounding whitespace and reject
        // control characters that could create a second logical line.
        ensure!(
            line.bytes().all(|b| b >= 0x20 || b == b'\t'),
            "invalid HLS line"
        );
        let line = line.trim_ascii();
        let before = out.len();
        if line.starts_with('#') {
            ensure!(
                !line.starts_with("#EXT-X-DEFINE:")
                    && !line.starts_with("#EXT-X-CONTENT-STEERING:"),
                "unsupported HLS playlist indirection"
            );
            if line.starts_with("#EXT-X-STREAM-INF:") {
                variant = true;
            }
            if let Some((tag, attrs)) = line.split_once(':')
                && tag.starts_with("#EXT-X-")
            {
                match tag {
                    // Scalar tags never consult the attribute parser.
                    "#EXT-X-VERSION"
                    | "#EXT-X-TARGETDURATION"
                    | "#EXT-X-MEDIA-SEQUENCE"
                    | "#EXT-X-DISCONTINUITY-SEQUENCE"
                    | "#EXT-X-PLAYLIST-TYPE"
                    | "#EXT-X-BYTERANGE"
                    | "#EXT-X-PROGRAM-DATE-TIME"
                    | "#EXT-X-BITRATE" => out.push_str(line),
                    "#EXT-X-STREAM-INF"
                    | "#EXT-X-KEY"
                    | "#EXT-X-MEDIA"
                    | "#EXT-X-MAP"
                    | "#EXT-X-START"
                    | "#EXT-X-DATERANGE"
                    | "#EXT-X-I-FRAME-STREAM-INF"
                    | "#EXT-X-IMAGE-STREAM-INF"
                    | "#EXT-X-SESSION-DATA"
                    | "#EXT-X-SESSION-KEY"
                    | "#EXT-X-PART"
                    | "#EXT-X-PART-INF"
                    | "#EXT-X-SERVER-CONTROL"
                    | "#EXT-X-SKIP"
                    | "#EXT-X-PRELOAD-HINT"
                    | "#EXT-X-RENDITION-REPORT" => {
                        out.push_str(tag);
                        out.push(':');
                        rewrite_attributes(
                            tag,
                            attrs,
                            base,
                            routes,
                            &mut rewritten,
                            &mut out,
                            extracted.as_deref_mut(),
                        )?;
                    }
                    // Preserve compatibility with private scalar extensions by
                    // skipping unrecognized tags before they reach the demuxer.
                    _ => continue,
                }
            } else {
                out.push_str(line);
            }
        } else if !line.is_empty() {
            let url = validate_stream_url(base.join(line)?.as_str())?;
            if let Some(sink) = extracted.as_deref_mut() {
                sink.push(url.as_str().to_owned());
            }
            let (kind, ext) = if variant {
                (RouteKind::Playlist, "m3u8")
            } else {
                (RouteKind::Media, "ts")
            };
            rewrite_uri(url, kind, ext, routes, &mut rewritten, &mut out)?;
            variant = false;
        }
        ensure!(
            out.len() - before < 4096,
            "HLS line exceeds the demuxer limit"
        );
        out.push('\n');
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
    }
    Ok(out)
}

/// Bounded, zero-network HLS playlist parser entry point for differential fuzz tests.
#[doc(hidden)]
pub mod fuzz_hls {
    use anyhow::Result;
    use url::Url;

    use super::super::Routes;

    /// Parses `input` against `base_url` via `iris-core`'s playlist rewriter and returns
    /// all extracted and validated upstream URIs in encounter order.
    pub fn parse_and_extract_uris(input: &str, base_url: &str) -> Result<Vec<String>> {
        let base = Url::parse(base_url)?;
        let routes = Routes::new("127.0.0.1:9898".parse().unwrap())?;
        let mut uris = Vec::new();
        let _ = super::rewrite_playlist_inner(input, &base, &routes, Some(&mut uris))?;
        Ok(uris)
    }
}

type RewrittenUris = BTreeMap<(Url, &'static str, &'static str), String>;

fn rewrite_uri(
    url: Url,
    kind: RouteKind,
    extension: &'static str,
    routes: &Routes,
    rewritten: &mut RewrittenUris,
    out: &mut String,
) -> Result<()> {
    // Byte ranges and keys can reference the same representation more than once.
    use std::collections::btree_map::Entry;
    let local = match rewritten.entry((url, kind.tag(), extension)) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let local = routes.seal(&entry.key().0, kind, extension)?;
            entry.insert(local)
        }
    };
    out.push_str(local);
    Ok(())
}

/// Rewrite complete URI attributes, respecting commas inside quoted values (RFC 8216 §4.2).
fn rewrite_attributes(
    tag: &str,
    mut attrs: &str,
    base: &Url,
    routes: &Routes,
    rewritten: &mut RewrittenUris,
    out: &mut String,
    mut extracted: Option<&mut Vec<String>>,
) -> Result<()> {
    // Accept an unambiguous subset of RFC 8216 §4.2. In particular, do not
    // pass whitespace-delimited attributes or quotes interpreted by libavformat.
    loop {
        let (key, rest) = attrs
            .split_once('=')
            .context("missing HLS attribute value")?;
        ensure!(
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'),
            "invalid HLS attribute name"
        );
        let (raw, tail, quoted) = if let Some(rest) = rest.strip_prefix('"') {
            let (value, tail) = rest.split_once('"').context("unterminated HLS attribute")?;
            ensure!(
                !value.contains(['\\', '\t']),
                "invalid quoted HLS attribute"
            );
            (value, tail, true)
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = &rest[..end];
            ensure!(
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|b| !b.is_ascii_whitespace() && !matches!(b, b'=' | b'"' | b'\\')),
                "invalid unquoted HLS attribute"
            );
            (value, &rest[end..], false)
        };
        let next = if tail.is_empty() {
            None
        } else {
            Some(
                tail.strip_prefix(',')
                    .filter(|s| !s.is_empty())
                    .context("invalid HLS attribute separator")?,
            )
        };
        out.push_str(key);
        out.push('=');
        if quoted {
            out.push('"');
        }
        if key == "URI" || key.ends_with("-URI") {
            ensure!(quoted && !raw.is_empty(), "invalid HLS URI attribute");
            let url = validate_stream_url(base.join(raw)?.as_str())?;
            if let Some(sink) = extracted.as_deref_mut() {
                sink.push(url.as_str().to_owned());
            }
            let (kind, fallback) = match tag {
                "#EXT-X-MEDIA"
                | "#EXT-X-I-FRAME-STREAM-INF"
                | "#EXT-X-IMAGE-STREAM-INF"
                | "#EXT-X-RENDITION-REPORT" => (RouteKind::Playlist, "m3u8"),
                "#EXT-X-KEY" | "#EXT-X-SESSION-KEY" => (RouteKind::Key, "key"),
                "#EXT-X-MAP" => (RouteKind::Media, "mp4"),
                _ => (RouteKind::Media, "ts"),
            };
            rewrite_uri(url, kind, fallback, routes, rewritten, out)?;
        } else {
            out.push_str(raw);
        }
        if quoted {
            out.push('"');
        }
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
        let Some(rest) = next else { break };
        out.push(',');
        attrs = rest;
    }
    Ok(())
}
