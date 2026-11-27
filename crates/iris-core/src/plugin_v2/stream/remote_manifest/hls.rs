use super::{Build, hls_parse, stable_id};
use crate::plugin_v2::stream::{range::decimal, remote::Fetched};
use iris_api::v2::{
    ErrorCode, WireU64,
    stream::{HlsTrack, HlsVariant, TrackKind, VirtualHls},
};
use std::collections::BTreeMap;
use url::Url;
type Attrs = BTreeMap<String, (String, bool)>;

pub(super) fn parse(fetched: &Fetched, build: &mut Build<'_>) -> Result<VirtualHls, ErrorCode> {
    let text = std::str::from_utf8(&fetched.bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?
        .trim_start_matches('\u{feff}');
    if !text.lines().any(|l| l.starts_with("#EXT-X-STREAM-INF:")) {
        let (track, live) = media(fetched, TrackKind::Video, "root", build)?;
        return Ok(VirtualHls {
            resources: std::mem::take(&mut build.resources).into_values().collect(),
            tracks: vec![track],
            variants: Vec::new(),
            live,
        });
    }
    let mut tracks: Vec<HlsTrack> = Vec::new();
    let mut variants = Vec::new();
    let mut audio = BTreeMap::<String, Option<String>>::new();
    let mut subtitle_group = None;
    let mut variant_attrs = None;
    let mut pending = Vec::new();
    let mut live = None;
    let mut lines = text.lines();
    if lines.next() != Some("#EXTM3U") {
        return Err(ErrorCode::InvalidArgument);
    }
    for line in lines {
        clean(line)?;
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            if tracks.len() >= 32 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let mut attrs = hls_parse::attributes(value)?;
            let kind = take(&mut attrs, "TYPE", false)?;
            let group = take(&mut attrs, "GROUP-ID", true)?;
            let label = take(&mut attrs, "NAME", true)?;
            let lang = optional(&mut attrs, "LANGUAGE", true)?;
            let default = yes_no(optional(&mut attrs, "DEFAULT", false)?)?.unwrap_or(false);
            if yes_no(optional(&mut attrs, "AUTOSELECT", false)?)? == Some(false)
                || yes_no(optional(&mut attrs, "FORCED", false)?)? == Some(true)
            {
                return Err(ErrorCode::Unsupported);
            }
            let _channels = optional(&mut attrs, "CHANNELS", true)?;
            let uri = optional(&mut attrs, "URI", true)?;
            if !attrs.is_empty() {
                return Err(ErrorCode::Unsupported);
            }
            let kind = match kind.as_str() {
                "AUDIO" => TrackKind::Audio,
                "SUBTITLES" => TrackKind::Subtitle,
                _ => return Err(ErrorCode::Unsupported),
            };
            if kind == TrackKind::Subtitle {
                if subtitle_group.as_ref().is_some_and(|g| g != &group) {
                    return Err(ErrorCode::Unsupported);
                }
                subtitle_group = Some(group.clone());
            }
            let Some(uri) = uri else {
                if kind != TrackKind::Audio || audio.insert(group, None).is_some() {
                    return Err(ErrorCode::Unsupported);
                }
                continue;
            };
            let child = build.child(&fetched.resource, &uri)?;
            let track_key = format!(
                "media:{}:{group}:{label}",
                if kind == TrackKind::Audio { "a" } else { "s" }
            );
            let (mut track, is_live) = media(&child, kind, &track_key, build)?;
            if tracks.iter().any(|t| t.id == track.id) {
                return Err(ErrorCode::InvalidArgument);
            }
            match live {
                Some(previous) if previous != is_live => return Err(ErrorCode::Unsupported),
                _ => live = Some(is_live),
            }
            track.label = label;
            track.lang = lang;
            track.default = default;
            if kind == TrackKind::Audio {
                let id = stable_id("group", &group);
                if audio.get(&group).is_some_and(Option::is_none) {
                    return Err(ErrorCode::Unsupported);
                }
                audio.insert(group, Some(id.clone()));
                track.group = Some(id);
            }
            tracks.push(track);
        } else if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if variant_attrs.is_some() {
                return Err(ErrorCode::InvalidArgument);
            }
            variant_attrs = Some(hls_parse::attributes(value)?);
        } else if !line.starts_with('#') {
            if pending.len() >= 64 {
                return Err(ErrorCode::QuotaExceeded);
            }
            pending.push((
                variant_attrs.take().ok_or(ErrorCode::InvalidArgument)?,
                line.to_owned(),
            ));
        } else if let Some(version) = line.strip_prefix("#EXT-X-VERSION:") {
            if !(1..=7).contains(&decimal(version)?) {
                return Err(ErrorCode::Unsupported);
            }
        } else if line != "#EXT-X-INDEPENDENT-SEGMENTS" {
            return Err(ErrorCode::Unsupported);
        }
    }
    if variant_attrs.is_some() || pending.is_empty() {
        return Err(ErrorCode::InvalidArgument);
    }
    for (index, (mut attrs, uri)) in pending.into_iter().enumerate() {
        if tracks.len() >= 32 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let bandwidth = WireU64(decimal(&take(&mut attrs, "BANDWIDTH", false)?)?);
        if bandwidth.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        if let Some(value) = optional(&mut attrs, "AVERAGE-BANDWIDTH", false)? {
            decimal(&value)?;
        }
        if let Some(value) = optional(&mut attrs, "FRAME-RATE", false)? {
            let rate: f64 = value.parse().map_err(|_| ErrorCode::InvalidArgument)?;
            if !rate.is_finite() || !(0.001..=1000.).contains(&rate) {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        let resolution = optional(&mut attrs, "RESOLUTION", false)?
            .map(|v| {
                let (w, h) = v.split_once('x').ok_or(ErrorCode::InvalidArgument)?;
                let w = u32::try_from(decimal(w)?).map_err(|_| ErrorCode::InvalidArgument)?;
                let h = u32::try_from(decimal(h)?).map_err(|_| ErrorCode::InvalidArgument)?;
                Ok::<_, ErrorCode>((w, h))
            })
            .transpose()?;
        let codec = optional(&mut attrs, "CODECS", true)?;
        let audio = optional(&mut attrs, "AUDIO", true)?
            .map(|g| audio.get(&g).cloned().ok_or(ErrorCode::InvalidArgument))
            .transpose()?
            .flatten();
        if let Some(group) = optional(&mut attrs, "SUBTITLES", true)?
            && subtitle_group.as_ref() != Some(&group)
        {
            return Err(ErrorCode::InvalidArgument);
        }
        if optional(&mut attrs, "CLOSED-CAPTIONS", false)?.is_some_and(|v| v != "NONE")
            || !attrs.is_empty()
        {
            return Err(ErrorCode::Unsupported);
        }
        let child = build.child(&fetched.resource, &uri)?;
        let track_key = format!("variant:{index}:{}", bandwidth.0);
        let (mut track, is_live) = media(&child, TrackKind::Video, &track_key, build)?;
        match live {
            Some(previous) if previous != is_live => return Err(ErrorCode::Unsupported),
            _ => live = Some(is_live),
        }
        track.codec = codec;
        variants.push(HlsVariant {
            video: track.id.clone(),
            audio,
            bandwidth,
            width: resolution.map(|n| n.0),
            height: resolution.map(|n| n.1),
        });
        if !tracks.iter().any(|t| t.id == track.id) {
            tracks.push(track);
        }
    }
    Ok(VirtualHls {
        resources: std::mem::take(&mut build.resources).into_values().collect(),
        tracks,
        variants,
        live: live.unwrap_or(false),
    })
}
fn media(
    fetched: &Fetched,
    kind: TrackKind,
    id_key: &str,
    build: &mut Build<'_>,
) -> Result<(HlsTrack, bool), ErrorCode> {
    let text = std::str::from_utf8(&fetched.bytes).map_err(|_| ErrorCode::InvalidArgument)?;
    let base = Url::parse(&fetched.resource.url).map_err(|_| ErrorCode::InvalidArgument)?;
    let (mut track, live) = hls_parse::parse_media(text, kind, |uri, res_kind, ext| {
        build.bind(&fetched.resource, &base, uri, res_kind, ext)
    })?;
    track.id = stable_id("track", id_key);
    track.kind = kind;
    track.default = false;
    Ok((track, live))
}
fn clean(line: &str) -> Result<(), ErrorCode> {
    if line.len() > 4096 || line.trim_ascii() != line || line.chars().any(char::is_control) {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
fn take(attrs: &mut Attrs, name: &str, quote: bool) -> Result<String, ErrorCode> {
    optional(attrs, name, quote)?.ok_or(ErrorCode::InvalidArgument)
}
fn optional(attrs: &mut Attrs, name: &str, quote: bool) -> Result<Option<String>, ErrorCode> {
    attrs
        .remove(name)
        .map(|(value, quoted)| {
            if quoted != quote {
                return Err(ErrorCode::InvalidArgument);
            }
            Ok(value)
        })
        .transpose()
}
fn yes_no(value: Option<String>) -> Result<Option<bool>, ErrorCode> {
    value
        .map(|v| match v.as_str() {
            "YES" => Ok(true),
            "NO" => Ok(false),
            _ => Err(ErrorCode::InvalidArgument),
        })
        .transpose()
}
