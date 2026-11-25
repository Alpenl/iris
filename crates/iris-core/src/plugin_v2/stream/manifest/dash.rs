use super::*;

pub(super) fn render(input: VirtualDash, build: &mut Build<'_>) -> Result<(), ErrorCode> {
    if input.duration_ms.0 == 0 || input.tracks.is_empty() || input.tracks.len() > 32 {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT{}.{:03}S\" minBufferTime=\"PT1S\"><Period start=\"PT0S\">",
        input.duration_ms.0 / 1000,
        input.duration_ms.0 % 1000
    );
    let mut ids = BTreeSet::new();
    let mut count = 0usize;
    for track in &input.tracks {
        logical(&track.id)?;
        attribute(&track.codec)?;
        if !ids.insert(&track.id)
            || track.bandwidth.0 == 0
            || track.timescale == 0
            || track.timescale > 1_000_000_000
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let (kind, types): (&str, &[&str]) = match track.kind {
            TrackKind::Video => ("video", &["video/mp4", "video/webm"]),
            TrackKind::Audio => ("audio", &["audio/mp4", "audio/webm"]),
            TrackKind::Subtitle => ("text", &["text/vtt", "application/mp4"]),
        };
        if !types.contains(&track.mime_type.as_str()) {
            return Err(ErrorCode::Unsupported);
        }
        append(&mut out, &format!("<AdaptationSet contentType=\"{kind}\""))?;
        if let Some(lang) = &track.lang {
            append(&mut out, &format!(" lang=\"{}\"", attribute(lang)?))?;
        }
        append(
            &mut out,
            &format!(
                "><Representation id=\"{}\" mimeType=\"{}\" codecs=\"{}\" bandwidth=\"{}\"",
                track.id, track.mime_type, track.codec, track.bandwidth.0
            ),
        )?;
        match (track.width, track.height) {
            (Some(w @ 1..=32768), Some(h @ 1..=32768)) => {
                append(&mut out, &format!(" width=\"{w}\" height=\"{h}\""))?
            }
            (None, None) => (),
            _ => return Err(ErrorCode::InvalidArgument),
        }
        if let Some(rate) = track.sample_rate {
            if !(8000..=768000).contains(&rate) {
                return Err(ErrorCode::InvalidArgument);
            }
            append(&mut out, &format!(" audioSamplingRate=\"{rate}\""))?;
        }
        append(&mut out, ">")?;
        match &track.segments {
            DashAddressing::SegmentBase { media, index } => {
                if media.range.is_some() {
                    return Err(ErrorCode::Unsupported);
                }
                let url = build.span(media, &[ResourceKind::Media])?;
                let end = range_end(index)?;
                if build.resources[&media.resource_id]
                    .size
                    .is_some_and(|n| end > n.0)
                {
                    return Err(ErrorCode::RangeInvalid);
                }
                append(
                    &mut out,
                    &format!(
                        "<BaseURL>{url}</BaseURL><SegmentBase timescale=\"{}\" presentationTimeOffset=\"{}\" indexRange=\"{}-{}\">",
                        track.timescale,
                        track.presentation_time_offset.0,
                        index.offset.0,
                        end - 1
                    ),
                )?;
                initialization(track.init.as_ref(), build, &mut out)?;
                append(&mut out, "</SegmentBase>")?;
            }
            addressing => {
                let segments = expand(addressing)?;
                count = count.saturating_add(segments.len());
                if segments.is_empty() || count > RESOURCE_LIMIT {
                    return Err(ErrorCode::QuotaExceeded);
                }
                append(
                    &mut out,
                    &format!(
                        "<SegmentList timescale=\"{}\" presentationTimeOffset=\"{}\">",
                        track.timescale, track.presentation_time_offset.0
                    ),
                )?;
                initialization(track.init.as_ref(), build, &mut out)?;
                append(&mut out, "<SegmentTimeline>")?;
                let mut previous = None;
                for segment in &segments {
                    if segment.duration.0 == 0 || previous.is_some_and(|end| segment.time.0 != end)
                    {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    previous = Some(
                        segment
                            .time
                            .0
                            .checked_add(segment.duration.0)
                            .ok_or(ErrorCode::RangeInvalid)?,
                    );
                    append(
                        &mut out,
                        &format!("<S t=\"{}\" d=\"{}\"/>", segment.time.0, segment.duration.0),
                    )?;
                }
                append(&mut out, "</SegmentTimeline>")?;
                for segment in segments {
                    let kinds: &[ResourceKind] = if track.kind == TrackKind::Subtitle {
                        &[ResourceKind::Subtitle]
                    } else {
                        &[ResourceKind::Media]
                    };
                    let url = build.span(&segment.resource, kinds)?;
                    append(&mut out, &format!("<SegmentURL media=\"{url}\""))?;
                    if let Some(range) = &segment.resource.range {
                        append(
                            &mut out,
                            &format!(
                                " mediaRange=\"{}-{}\"",
                                range.offset.0,
                                range_end(range)? - 1
                            ),
                        )?;
                    }
                    append(&mut out, "/>")?;
                }
                append(&mut out, "</SegmentList>")?;
            }
        }
        append(&mut out, "</Representation></AdaptationSet>")?;
    }
    append(&mut out, "</Period></MPD>")?;
    build.add(ROOT, out)
}
fn initialization(
    span: Option<&ResourceSpan>,
    build: &mut Build<'_>,
    out: &mut String,
) -> Result<(), ErrorCode> {
    if let Some(span) = span {
        let url = build.span(span, &[ResourceKind::Init])?;
        append(out, &format!("<Initialization sourceURL=\"{url}\""))?;
        if let Some(range) = &span.range {
            append(
                out,
                &format!(" range=\"{}-{}\"", range.offset.0, range_end(range)? - 1),
            )?;
        }
        append(out, "/>")?;
    }
    Ok(())
}
fn expand(addressing: &DashAddressing) -> Result<Vec<DashSegment>, ErrorCode> {
    match addressing {
        DashAddressing::SegmentList { segments } => {
            if segments.len() > RESOURCE_LIMIT {
                return Err(ErrorCode::QuotaExceeded);
            }
            Ok(segments.clone())
        }
        DashAddressing::SegmentTemplate {
            resource_template,
            start_number,
            timeline,
        } => {
            if timeline.len() > RESOURCE_LIMIT || resource_template.len() > 128 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let symbol = if resource_template.contains("$Number$") {
                "$Number$"
            } else {
                "$Time$"
            };
            if resource_template.matches(symbol).count() != 1
                || resource_template.replace(symbol, "0").contains('$')
            {
                return Err(ErrorCode::InvalidArgument);
            }
            logical(&resource_template.replace(symbol, "0"))?;
            let mut out = Vec::new();
            let mut time = 0u64;
            for entry in timeline {
                if entry.duration.0 == 0
                    || entry.repeat as usize >= RESOURCE_LIMIT
                    || out.len() + entry.repeat as usize + 1 > RESOURCE_LIMIT
                {
                    return Err(ErrorCode::QuotaExceeded);
                }
                if let Some(start) = entry.time {
                    time = start.0;
                }
                for _ in 0..=entry.repeat {
                    let number = start_number
                        .0
                        .checked_add(out.len() as u64)
                        .ok_or(ErrorCode::RangeInvalid)?;
                    let id = resource_template.replace(
                        symbol,
                        &if symbol == "$Number$" { number } else { time }.to_string(),
                    );
                    logical(&id)?;
                    out.push(DashSegment {
                        resource: ResourceSpan {
                            resource_id: id,
                            range: None,
                        },
                        time: iris_api::v2::Id(time),
                        duration: entry.duration,
                    });
                    time = time
                        .checked_add(entry.duration.0)
                        .ok_or(ErrorCode::RangeInvalid)?;
                }
            }
            Ok(out)
        }
        DashAddressing::SegmentBase { .. } => Err(ErrorCode::BadState),
    }
}
