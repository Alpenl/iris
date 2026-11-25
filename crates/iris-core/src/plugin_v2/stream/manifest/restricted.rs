//! Restricted media-playlist grammar. Master playlists use VirtualHls instead;
//! no arbitrary attribute or unknown tag can survive serialization.
use super::*;
use iris_api::v2::Id;

pub(super) fn render(text: &str, build: &mut Build<'_>) -> Result<(), ErrorCode> {
    let (track, live) = parse(text, &build.resources)?;
    hls::render(
        VirtualHls {
            resources: Vec::new(),
            tracks: vec![track],
            variants: Vec::new(),
            live,
        },
        build,
    )
}

pub(crate) fn parse(
    text: &str,
    resources: &BTreeMap<String, ResourceBinding>,
) -> Result<(HlsTrack, bool), ErrorCode> {
    if text.len() > INPUT_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next() != Some("#EXTM3U") {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut track = HlsTrack {
        id: "media".into(),
        kind: TrackKind::Video,
        group: None,
        label: "Media".into(),
        lang: None,
        codec: None,
        default: true,
        sequence: Id(0),
        discontinuity_sequence: Id(0),
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
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.len() > 4096
            || line.trim_ascii() != line
            || line.chars().any(char::is_control)
            || ended
        {
            return Err(ErrorCode::InvalidArgument);
        }
        if !line.starts_with('#') {
            logical(line)?;
            if track.segments.len() >= RESOURCE_LIMIT {
                return Err(ErrorCode::QuotaExceeded);
            }
            let resource = resources.get(line).ok_or(ErrorCode::InvalidArgument)?;
            if track.segments.is_empty() && resource.kind == ResourceKind::Subtitle {
                track.kind = TrackKind::Subtitle;
            }
            let range = range
                .take()
                .map(|(len, offset): (u64, Option<u64>)| {
                    let offset = match offset {
                        Some(n) => n,
                        None => {
                            let previous = track
                                .segments
                                .last()
                                .filter(|s| s.resource.resource_id == line)
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
                        offset: Id(offset),
                        len: Id(len),
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
                resource: ResourceSpan {
                    resource_id: line.into(),
                    range,
                },
                duration_ms: Id(duration.take().ok_or(ErrorCode::InvalidArgument)?),
                init: init.clone(),
                key: key.as_ref().map(|(resource_id, iv)| HlsKey {
                    resource_id: resource_id.clone(),
                    iv: iv.clone().unwrap_or_else(|| format!("{sequence:032x}")),
                }),
                discontinuity: std::mem::take(&mut discontinuity),
            });
            continue;
        }
        let (tag, value) = line
            .split_once(':')
            .map_or((line, None), |(tag, value)| (tag, Some(value)));
        match tag {
            "#EXT-X-VERSION"
            | "#EXT-X-TARGETDURATION"
            | "#EXT-X-MEDIA-SEQUENCE"
            | "#EXT-X-DISCONTINUITY-SEQUENCE"
            | "#EXT-X-PLAYLIST-TYPE" => {
                if !seen.insert(tag) || !track.segments.is_empty() {
                    return Err(ErrorCode::InvalidArgument);
                }
                let value = value.ok_or(ErrorCode::InvalidArgument)?;
                match tag {
                    "#EXT-X-VERSION" => {
                        if !(1..=7).contains(&super::super::range::decimal(value)?) {
                            return Err(ErrorCode::Unsupported);
                        }
                    }
                    "#EXT-X-TARGETDURATION" => {
                        let n = super::super::range::decimal(value)?;
                        if n == 0 || n > 86400 {
                            return Err(ErrorCode::InvalidArgument);
                        }
                        target = Some(n);
                    }
                    "#EXT-X-MEDIA-SEQUENCE" => {
                        track.sequence = Id(super::super::range::decimal(value)?)
                    }
                    "#EXT-X-DISCONTINUITY-SEQUENCE" => {
                        track.discontinuity_sequence = Id(super::super::range::decimal(value)?)
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
                let (seconds, title) = value
                    .and_then(|v| v.split_once(','))
                    .ok_or(ErrorCode::InvalidArgument)?;
                if title.len() > 256 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                duration = Some(milliseconds(seconds)?);
            }
            "#EXT-X-BYTERANGE" => {
                if range.is_some() {
                    return Err(ErrorCode::InvalidArgument);
                }
                range = Some(byte_range(value.ok_or(ErrorCode::InvalidArgument)?)?);
            }
            "#EXT-X-MAP" => {
                let mut attrs = attributes(value.ok_or(ErrorCode::InvalidArgument)?)?;
                let resource_id = take(&mut attrs, "URI", true)?;
                logical(&resource_id)?;
                let range = attrs
                    .remove("BYTERANGE")
                    .map(|(v, quoted)| {
                        if !quoted {
                            return Err(ErrorCode::InvalidArgument);
                        }
                        let (len, offset) = byte_range(&v)?;
                        Ok(ByteRange {
                            len: Id(len),
                            offset: Id(offset.ok_or(ErrorCode::RangeInvalid)?),
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
                    logical(&uri)?;
                    let iv = attrs
                        .remove("IV")
                        .map(|(v, quoted)| {
                            let v = v.strip_prefix("0x").ok_or(ErrorCode::InvalidArgument)?;
                            if quoted || v.len() != 32 || !v.bytes().all(|b| b.is_ascii_hexdigit())
                            {
                                return Err(ErrorCode::InvalidArgument);
                            }
                            Ok::<_, ErrorCode>(v.to_owned())
                        })
                        .transpose()?;
                    for (k, expected) in [("KEYFORMAT", "identity"), ("KEYFORMATVERSIONS", "1")] {
                        if let Some((v, quoted)) = attrs.remove(k)
                            && (!quoted || v != expected)
                        {
                            return Err(ErrorCode::Unsupported);
                        }
                    }
                    Some((uri, iv))
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
                if value.is_some() || !seen.insert(tag) {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            _ => return Err(ErrorCode::Unsupported),
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
            .any(|s| s.duration_ms.0.div_ceil(1000) > target)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok((track, !ended))
}

fn milliseconds(value: &str) -> Result<u64, ErrorCode> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    // FFmpeg emits six fractional places even for exact milliseconds. Accept
    // insignificant zero padding without rounding non-representable durations.
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ErrorCode::InvalidArgument);
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > 3 {
        return Err(ErrorCode::Unsupported);
    }
    let whole = super::super::range::decimal(whole)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        super::super::range::decimal(fraction)? * 10u64.pow(3 - fraction.len() as u32)
    };
    let n = whole
        .checked_mul(1000)
        .and_then(|n| n.checked_add(fraction))
        .ok_or(ErrorCode::RangeInvalid)?;
    if n == 0 || n > 86_400_000 {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(n)
}
fn byte_range(value: &str) -> Result<(u64, Option<u64>), ErrorCode> {
    let (len, offset) = value
        .split_once('@')
        .map_or((value, None), |(len, offset)| (len, Some(offset)));
    let len = super::super::range::decimal(len)?;
    if len == 0 {
        return Err(ErrorCode::RangeInvalid);
    }
    Ok((len, offset.map(super::super::range::decimal).transpose()?))
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
pub(crate) fn attributes(mut input: &str) -> Result<BTreeMap<String, (String, bool)>, ErrorCode> {
    let mut out = BTreeMap::new();
    loop {
        let (name, rest) = input.split_once('=').ok_or(ErrorCode::InvalidArgument)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let (value, tail, quoted) = if let Some(rest) = rest.strip_prefix('"') {
            let (v, tail) = rest.split_once('"').ok_or(ErrorCode::InvalidArgument)?;
            if v.is_empty() || v.len() > 4096 || v.chars().any(|c| c.is_control() || c == '\\') {
                return Err(ErrorCode::InvalidArgument);
            }
            (v, tail, true)
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let v = &rest[..end];
            if v.is_empty()
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
            {
                return Err(ErrorCode::InvalidArgument);
            }
            (v, &rest[end..], false)
        };
        if out.len() >= 16 || out.insert(name.into(), (value.into(), quoted)).is_some() {
            return Err(ErrorCode::InvalidArgument);
        }
        if tail.is_empty() {
            return Ok(out);
        }
        input = tail
            .strip_prefix(',')
            .filter(|s| !s.is_empty())
            .ok_or(ErrorCode::InvalidArgument)?;
    }
}
