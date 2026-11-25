use super::{Build, RESOURCE_LIMIT, ROOT, append, attribute, logical};
use iris_api::v2::{
    ErrorCode,
    stream::{HlsTrack, ResourceKind, TrackKind, VirtualHls},
};
use std::collections::BTreeMap;

pub(super) fn render(input: VirtualHls, build: &mut Build<'_>) -> Result<(), ErrorCode> {
    if input.tracks.is_empty() || input.tracks.len() > 32 || input.variants.len() > 64 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut tracks = BTreeMap::new();
    let mut groups = BTreeMap::<&str, Vec<&HlsTrack>>::new();
    let mut count = 0usize;
    for track in &input.tracks {
        logical(&track.id)?;
        if let Some(group) = &track.group {
            logical(group)?;
        }
        if track.kind == TrackKind::Audio {
            groups
                .entry(track.group.as_deref().unwrap_or(&track.id))
                .or_default()
                .push(track);
        } else if track.group.is_some() {
            return Err(ErrorCode::Unsupported);
        }
        attribute(&track.label)?;
        if let Some(lang) = &track.lang {
            attribute(lang)?;
        }
        if let Some(codec) = &track.codec {
            attribute(codec)?;
        }
        count = count.saturating_add(track.segments.len());
        if track.segments.is_empty()
            || count > RESOURCE_LIMIT
            || tracks.insert(track.id.as_str(), track).is_some()
        {
            return Err(ErrorCode::InvalidArgument);
        }
        track
            .sequence
            .0
            .checked_add(track.segments.len() as u64)
            .ok_or(ErrorCode::RangeInvalid)?;
    }
    if input.tracks.len() == 1 && input.variants.is_empty() {
        return render_track(ROOT, &input.tracks[0], input.live, build);
    }
    if input.variants.is_empty() {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut master = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    let mut urls = BTreeMap::new();
    for track in &input.tracks {
        let id = format!("__iris_hls_{}", track.id);
        render_track(&id, track, input.live, build)?;
        let url = build.document_url(&id)?;
        urls.insert(track.id.as_str(), url.clone());
        if track.kind != TrackKind::Video {
            let (kind, group) = match track.kind {
                TrackKind::Audio => ("AUDIO", track.group.as_deref().unwrap_or(&track.id)),
                TrackKind::Subtitle => ("SUBTITLES", "subtitles"),
                TrackKind::Video => unreachable!(),
            };
            append(
                &mut master,
                &format!(
                    "#EXT-X-MEDIA:TYPE={kind},GROUP-ID=\"{group}\",NAME=\"{}\",DEFAULT={},AUTOSELECT=YES,URI=\"{url}\"",
                    track.label,
                    if track.default { "YES" } else { "NO" }
                ),
            )?;
            if let Some(lang) = &track.lang {
                append(&mut master, &format!(",LANGUAGE=\"{lang}\""))?;
            }
            master.push('\n');
        }
    }
    if groups
        .values()
        .any(|tracks| tracks.iter().filter(|t| t.default).count() > 1)
        || input
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.default)
            .count()
            > 1
    {
        return Err(ErrorCode::InvalidArgument);
    }
    for variant in &input.variants {
        let video = tracks
            .get(variant.video.as_str())
            .ok_or(ErrorCode::InvalidArgument)?;
        if video.kind == TrackKind::Subtitle || variant.bandwidth.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        append(
            &mut master,
            &format!("#EXT-X-STREAM-INF:BANDWIDTH={}", variant.bandwidth.0),
        )?;
        let mut codecs = video.codec.clone().unwrap_or_default();
        if let Some(audio) = &variant.audio {
            let group = groups
                .get(audio.as_str())
                .ok_or(ErrorCode::InvalidArgument)?;
            append(&mut master, &format!(",AUDIO=\"{audio}\""))?;
            for track in group {
                if let Some(codec) = &track.codec
                    && !codecs.split(',').any(|s| s == codec)
                {
                    if !codecs.is_empty() {
                        codecs.push(',');
                    }
                    codecs.push_str(codec);
                }
            }
        }
        if !codecs.is_empty() {
            append(&mut master, &format!(",CODECS=\"{codecs}\""))?;
        }
        match (variant.width, variant.height) {
            (Some(w @ 1..=32768), Some(h @ 1..=32768)) => {
                append(&mut master, &format!(",RESOLUTION={w}x{h}"))?
            }
            (None, None) => (),
            _ => return Err(ErrorCode::InvalidArgument),
        }
        if input.tracks.iter().any(|t| t.kind == TrackKind::Subtitle) {
            append(&mut master, ",SUBTITLES=\"subtitles\"")?;
        }
        append(
            &mut master,
            &format!("\n{}\n", urls[variant.video.as_str()]),
        )?;
    }
    build.add(ROOT, master)
}

fn render_track(
    id: &str,
    track: &HlsTrack,
    live: bool,
    build: &mut Build<'_>,
) -> Result<(), ErrorCode> {
    let max = track
        .segments
        .iter()
        .map(|s| s.duration_ms.0)
        .max()
        .ok_or(ErrorCode::InvalidArgument)?;
    if max == 0 || max > 86_400_000 {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut text = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
        max.div_ceil(1000),
        track.sequence.0,
        track.discontinuity_sequence.0
    );
    let mut init = None;
    let mut key = None;
    for segment in &track.segments {
        if segment.duration_ms.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        if segment.discontinuity {
            append(&mut text, "#EXT-X-DISCONTINUITY\n")?;
        }
        if key != segment.key.as_ref() {
            if let Some(new) = &segment.key {
                if new.iv.len() != 32 || !new.iv.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(ErrorCode::InvalidArgument);
                }
                let url = build.resource(&new.resource_id, &[ResourceKind::Key])?;
                append(
                    &mut text,
                    &format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{url}\",IV=0x{}\n", new.iv),
                )?;
            } else {
                append(&mut text, "#EXT-X-KEY:METHOD=NONE\n")?;
            }
            key = segment.key.as_ref();
        }
        if init != segment.init.as_ref() {
            let Some(new) = &segment.init else {
                return Err(ErrorCode::Unsupported);
            };
            let url = build.span(new, &[ResourceKind::Init])?;
            append(&mut text, &format!("#EXT-X-MAP:URI=\"{url}\""))?;
            if let Some(range) = &new.range {
                append(
                    &mut text,
                    &format!(",BYTERANGE=\"{}@{}\"", range.len.0, range.offset.0),
                )?;
            }
            append(&mut text, "\n")?;
            init = Some(new);
        }
        let kinds: &[ResourceKind] = if track.kind == TrackKind::Subtitle {
            &[ResourceKind::Subtitle]
        } else {
            &[ResourceKind::Media]
        };
        let url = build.span(&segment.resource, kinds)?;
        if let Some(range) = &segment.resource.range {
            append(
                &mut text,
                &format!("#EXT-X-BYTERANGE:{}@{}\n", range.len.0, range.offset.0),
            )?;
        }
        append(
            &mut text,
            &format!(
                "#EXTINF:{}.{:03},\n{url}\n",
                segment.duration_ms.0 / 1000,
                segment.duration_ms.0 % 1000
            ),
        )?;
    }
    if !live {
        append(&mut text, "#EXT-X-ENDLIST\n")?;
    }
    build.add(id, text)
}
