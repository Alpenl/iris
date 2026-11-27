//! Strict RFC 8216 subset policy and sealed-URI rewriter for authenticated HLS manifests,
//! backed by `iris_base::net::hls` syntax parsing.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, ensure};
use url::Url;

use iris_base::net::hls::{
    AttributeOptions, HlsLine, HlsLineOptions, has_hls_magic, parse_attributes, parse_lines,
};
use iris_base::net::http_client::validate_stream_url;

use super::{RouteKind, Routes};

const MAX_REWRITTEN_PLAYLIST: usize = 16 * 1024 * 1024;

pub(super) fn is_hls(prefix: &[u8]) -> bool {
    has_hls_magic(prefix)
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
    let lines = parse_lines(input, HlsLineOptions::CORE_REWRITE).map_err(|e| anyhow!(e))?;
    let mut out = String::with_capacity(input.len());
    out.push_str("#EXTM3U\n");
    ensure!(
        out.len() <= MAX_REWRITTEN_PLAYLIST,
        "rewritten playlist exceeds limit"
    );

    let mut variant = false;
    let mut rewritten = BTreeMap::new();
    for item in lines {
        let line = item.map_err(|e| anyhow!(e))?;
        let before = out.len();
        match line {
            HlsLine::Empty => {}
            HlsLine::Comment(raw) => out.push_str(raw),
            HlsLine::Tag { raw, name, value } => {
                ensure!(
                    !(matches!(name, "#EXT-X-DEFINE" | "#EXT-X-CONTENT-STEERING")
                        && value.is_some()),
                    "unsupported HLS playlist indirection"
                );
                if name == "#EXT-X-STREAM-INF" && value.is_some() {
                    variant = true;
                }
                if let Some(attrs) = value
                    && name.starts_with("#EXT-X-")
                {
                    match name {
                        // Scalar tags never consult the attribute parser.
                        "#EXT-X-VERSION"
                        | "#EXT-X-TARGETDURATION"
                        | "#EXT-X-MEDIA-SEQUENCE"
                        | "#EXT-X-DISCONTINUITY-SEQUENCE"
                        | "#EXT-X-PLAYLIST-TYPE"
                        | "#EXT-X-BYTERANGE"
                        | "#EXT-X-PROGRAM-DATE-TIME"
                        | "#EXT-X-BITRATE" => out.push_str(raw),
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
                            out.push_str(name);
                            out.push(':');
                            rewrite_attributes(
                                name,
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
                    out.push_str(raw);
                }
            }
            HlsLine::Uri(reference) => {
                let url = validate_stream_url(base.join(reference)?.as_str())?;
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

/// Rewrite complete URI attributes using `iris_base::net::hls::parse_attributes` (RFC 8216 §4.2).
fn rewrite_attributes(
    tag: &str,
    attrs: &str,
    base: &Url,
    routes: &Routes,
    rewritten: &mut RewrittenUris,
    out: &mut String,
    mut extracted: Option<&mut Vec<String>>,
) -> Result<()> {
    let parsed = parse_attributes(attrs, AttributeOptions::CORE_REWRITE).map_err(|e| anyhow!(e))?;
    for (idx, attr) in parsed.into_iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(attr.name);
        out.push('=');
        if attr.quoted {
            out.push('"');
        }
        if attr.name == "URI" || attr.name.ends_with("-URI") {
            ensure!(
                attr.quoted && !attr.value.is_empty(),
                "invalid HLS URI attribute"
            );
            let url = validate_stream_url(base.join(attr.value)?.as_str())?;
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
            out.push_str(attr.value);
        }
        if attr.quoted {
            out.push('"');
        }
        ensure!(
            out.len() <= MAX_REWRITTEN_PLAYLIST,
            "rewritten playlist exceeds limit"
        );
    }
    Ok(())
}
