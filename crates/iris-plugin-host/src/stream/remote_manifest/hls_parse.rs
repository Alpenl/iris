//! Bounded remote HLS media-playlist conversion policy over `iris_base::net::hls`.
//! Only known tags and explicitly bound local resources survive conversion.
use crate::stream::{
    manifest::{INPUT_LIMIT, RESOURCE_LIMIT, logical},
    range::decimal,
};
use iris_api::v2::{
    ErrorCode, WireU64,
    stream::{
        ByteRange, HlsKey, HlsSegment, HlsTrack, ResourceExtension, ResourceKind, ResourceSpan,
        TrackKind,
    },
};
use iris_base::net::hls::{
    AttributeOptions, HlsError, HlsLine, HlsLineOptions, parse_attribute_map, parse_byterange,
    parse_extinf, parse_hex_iv_128, parse_lines, parse_version,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const fn map_hls_error(err: HlsError) -> ErrorCode {
    match err {
        HlsError::InvalidSyntax(_) => ErrorCode::InvalidArgument,
        HlsError::NumberInvalid(_) => ErrorCode::RangeInvalid,
        HlsError::QuotaExceeded(_) => ErrorCode::QuotaExceeded,
        HlsError::Unsupported(_) => ErrorCode::Unsupported,
    }
}

pub fn parse_media(
    text: &str,
    kind: TrackKind,
    mut bind: impl FnMut(&str, ResourceKind, ResourceExtension) -> Result<String, ErrorCode>,
) -> Result<(HlsTrack, bool), ErrorCode> {
    let lines =
        parse_lines(text, HlsLineOptions::strict_remote(INPUT_LIMIT)).map_err(map_hls_error)?;
    if lines.any_line_starts_with("#EXT-X-STREAM-INF:")
        || lines.any_line_starts_with("#EXT-X-MEDIA:")
    {
        return Err(ErrorCode::Unsupported);
    }
    let fragmented = lines.any_line_starts_with("#EXT-X-MAP:");
    let mut track = HlsTrack {
        id: "media".into(),
        kind,
        group: None,
        label: "Media".into(),
        lang: None,
        codec: None,
        default: true,
        sequence: WireU64(0),
        discontinuity_sequence: WireU64(0),
        segments: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let mut target = None;
    let mut duration = None;
    let mut range = None;
    let mut init = None;
    let mut key: Option<(String, Option<String>)> = None;
    let mut discontinuity = false;
    let mut ended = false;
    for item in lines {
        let line = item.map_err(map_hls_error)?;
        if matches!(line, HlsLine::Empty) {
            continue;
        }
        if ended {
            return Err(ErrorCode::InvalidArgument);
        }
        match line {
            HlsLine::Empty => unreachable!(),
            HlsLine::Comment(_) => return Err(ErrorCode::Unsupported),
            HlsLine::Uri(uri_ref) => {
                if track.segments.len() >= RESOURCE_LIMIT {
                    return Err(ErrorCode::QuotaExceeded);
                }
                let (res_kind, res_ext) = match kind {
                    TrackKind::Subtitle => (ResourceKind::Subtitle, ResourceExtension::Vtt),
                    _ => (
                        ResourceKind::Media,
                        if fragmented {
                            ResourceExtension::M4s
                        } else {
                            ResourceExtension::Ts
                        },
                    ),
                };
                let resource_id = bind(uri_ref, res_kind, res_ext)?;
                logical(&resource_id)?;
                let range = range
                    .take()
                    .map(|(len, offset): (u64, Option<u64>)| {
                        let offset = match offset {
                            Some(n) => n,
                            None => {
                                let previous = track
                                    .segments
                                    .last()
                                    .filter(|s| s.resource.resource_id == resource_id)
                                    .ok_or(ErrorCode::RangeInvalid)?;
                                range_end(
                                    previous
                                        .resource
                                        .range
                                        .as_ref()
                                        .ok_or(ErrorCode::RangeInvalid)?,
                                )?
                            }
                        };
                        let range = ByteRange {
                            offset: WireU64(offset),
                            len: WireU64(len),
                        };
                        range_end(&range)?;
                        Ok::<_, ErrorCode>(range)
                    })
                    .transpose()?;
                let sequence = track
                    .sequence
                    .0
                    .checked_add(track.segments.len() as u64)
                    .ok_or(ErrorCode::RangeInvalid)?;
                if init.is_some() && key.as_ref().is_some_and(|(_, iv)| iv.is_none()) {
                    return Err(ErrorCode::InvalidArgument);
                }
                track.segments.push(HlsSegment {
                    resource: ResourceSpan { resource_id, range },
                    duration_ms: WireU64(duration.take().ok_or(ErrorCode::InvalidArgument)?),
                    init: init.clone(),
                    key: key.as_ref().map(|(resource_id, iv)| HlsKey {
                        resource_id: resource_id.clone(),
                        iv: iv.clone().unwrap_or_else(|| format!("{sequence:032x}")),
                    }),
                    discontinuity: std::mem::take(&mut discontinuity),
                });
            }
            HlsLine::Tag { name, value, .. } => match name {
                "#EXT-X-VERSION"
                | "#EXT-X-TARGETDURATION"
                | "#EXT-X-MEDIA-SEQUENCE"
                | "#EXT-X-DISCONTINUITY-SEQUENCE"
                | "#EXT-X-PLAYLIST-TYPE" => {
                    if !seen.insert(name) || !track.segments.is_empty() {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let value = value.ok_or(ErrorCode::InvalidArgument)?;
                    match name {
                        "#EXT-X-VERSION" => {
                            parse_version(value).map_err(map_hls_error)?;
                        }
                        "#EXT-X-TARGETDURATION" => {
                            let n = decimal(value)?;
                            if n == 0 || n > 86400 {
                                return Err(ErrorCode::InvalidArgument);
                            }
                            target = Some(n);
                        }
                        "#EXT-X-MEDIA-SEQUENCE" => track.sequence = WireU64(decimal(value)?),
                        "#EXT-X-DISCONTINUITY-SEQUENCE" => {
                            track.discontinuity_sequence = WireU64(decimal(value)?)
                        }
                        _ => {
                            if !matches!(value, "VOD" | "EVENT") {
                                return Err(ErrorCode::InvalidArgument);
                            }
                        }
                    }
                }
                "#EXTINF" => {
                    if duration.is_some() {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let raw = value.ok_or(ErrorCode::InvalidArgument)?;
                    let (ms, _) = parse_extinf(raw, Some(256)).map_err(map_hls_error)?;
                    duration = Some(ms);
                }
                "#EXT-X-BYTERANGE" => {
                    if range.is_some() {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let raw = value.ok_or(ErrorCode::InvalidArgument)?;
                    range = Some(parse_byterange(raw).map_err(map_hls_error)?);
                }
                "#EXT-X-MAP" => {
                    let mut attrs = attributes(value.ok_or(ErrorCode::InvalidArgument)?)?;
                    let uri = take(&mut attrs, "URI", true)?;
                    let resource_id = bind(&uri, ResourceKind::Init, ResourceExtension::Mp4)?;
                    logical(&resource_id)?;
                    let range = attrs
                        .remove("BYTERANGE")
                        .map(|(v, quoted)| {
                            if !quoted {
                                return Err(ErrorCode::InvalidArgument);
                            }
                            let (len, offset) = parse_byterange(&v).map_err(map_hls_error)?;
                            Ok(ByteRange {
                                len: WireU64(len),
                                offset: WireU64(offset.ok_or(ErrorCode::RangeInvalid)?),
                            })
                        })
                        .transpose()?;
                    if !attrs.is_empty() {
                        return Err(ErrorCode::Unsupported);
                    }
                    init = Some(ResourceSpan { resource_id, range });
                }
                "#EXT-X-KEY" => {
                    let mut attrs = attributes(value.ok_or(ErrorCode::InvalidArgument)?)?;
                    let method = take(&mut attrs, "METHOD", false)?;
                    key = if method == "NONE" {
                        None
                    } else {
                        if method != "AES-128" {
                            return Err(ErrorCode::Unsupported);
                        }
                        let uri = take(&mut attrs, "URI", true)?;
                        let resource_id = bind(&uri, ResourceKind::Key, ResourceExtension::Key)?;
                        logical(&resource_id)?;
                        let iv = attrs
                            .remove("IV")
                            .map(|(v, quoted)| {
                                parse_hex_iv_128(&v, quoted)
                                    .map(str::to_owned)
                                    .map_err(map_hls_error)
                            })
                            .transpose()?;
                        for (k, expected) in [("KEYFORMAT", "identity"), ("KEYFORMATVERSIONS", "1")]
                        {
                            if let Some((v, quoted)) = attrs.remove(k)
                                && (!quoted || v != expected)
                            {
                                return Err(ErrorCode::Unsupported);
                            }
                        }
                        Some((resource_id, iv))
                    };
                    if !attrs.is_empty() {
                        return Err(ErrorCode::Unsupported);
                    }
                }
                "#EXT-X-DISCONTINUITY" => {
                    if value.is_some() || discontinuity {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    discontinuity = true;
                }
                "#EXT-X-ENDLIST" => {
                    if value.is_some() {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    ended = true;
                }
                "#EXT-X-INDEPENDENT-SEGMENTS" => {
                    if value.is_some() || !seen.insert(name) {
                        return Err(ErrorCode::InvalidArgument);
                    }
                }
                _ => return Err(ErrorCode::Unsupported),
            },
        }
    }
    let target = target.ok_or(ErrorCode::InvalidArgument)?;
    if duration.is_some()
        || range.is_some()
        || discontinuity
        || track.segments.is_empty()
        || track
            .segments
            .iter()
            // RFC 8216 §4.3.3.1 compares durations rounded to the nearest second.
            // Parsed durations are bounded to 24 hours, so adding 500 is safe.
            .any(|s| (s.duration_ms.0 + 500) / 1000 > target)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok((track, !ended))
}

fn range_end(range: &ByteRange) -> Result<u64, ErrorCode> {
    if range.len.0 == 0 {
        return Err(ErrorCode::RangeInvalid);
    }
    range
        .offset
        .0
        .checked_add(range.len.0)
        .ok_or(ErrorCode::RangeInvalid)
}

fn take(
    attrs: &mut BTreeMap<String, (String, bool)>,
    name: &str,
    quoted: bool,
) -> Result<String, ErrorCode> {
    let (value, q) = attrs.remove(name).ok_or(ErrorCode::InvalidArgument)?;
    if q != quoted {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(value)
}

pub fn attributes(input: &str) -> Result<BTreeMap<String, (String, bool)>, ErrorCode> {
    parse_attribute_map(input, AttributeOptions::STRICT_REMOTE).map_err(map_hls_error)
}
