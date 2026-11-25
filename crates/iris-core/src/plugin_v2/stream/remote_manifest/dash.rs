mod timeline;
use super::*;
use crate::plugin_v2::stream::range::decimal;
use iris_api::v2::Id;
use roxmltree::Node;
const NS: &str = "urn:mpeg:dash:schema:mpd:2011";

pub(super) fn parse(fetched: &Fetched, build: &mut Build<'_>) -> Result<VirtualDash, ErrorCode> {
    let text = std::str::from_utf8(&fetched.bytes).map_err(|_| ErrorCode::InvalidArgument)?;
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 50_000,
        },
    )
    .map_err(|_| ErrorCode::InvalidArgument)?;
    let root = doc.root_element();
    if !root.has_tag_name((NS, "MPD")) {
        return Err(ErrorCode::Unsupported);
    }
    node(
        root,
        &[
            "id",
            "type",
            "profiles",
            "mediaPresentationDuration",
            "minBufferTime",
            "maxSegmentDuration",
        ],
        &[
            "BaseURL",
            "Period",
            "ProgramInformation",
            "ServiceDescription",
        ],
    )?;
    // These metadata nodes never become references or reach the generated MPD.
    // Non-empty service semantics still require an explicit supported profile.
    if let Some(info) = one(root, "ProgramInformation")? {
        node(info, &[], &["Title", "Source", "Copyright"])?;
        for child in info.children().filter(Node::is_element) {
            node(child, &[], &[])?;
        }
    }
    if let Some(service) = one(root, "ServiceDescription")? {
        node(service, &["id"], &[])?;
    }
    if root.attribute("type").is_some_and(|v| v != "static") {
        return Err(ErrorCode::Unsupported);
    }
    let duration = timeline::duration(need(root, "mediaPresentationDuration")?)?;
    if duration == 0 {
        return Err(ErrorCode::InvalidArgument);
    }
    for attr in ["minBufferTime", "maxSegmentDuration"] {
        if let Some(v) = root.attribute(attr) {
            timeline::duration(v)?;
        }
    }
    let period = one(root, "Period")?.ok_or(ErrorCode::InvalidArgument)?;
    node(
        period,
        &["id", "start", "duration"],
        &["BaseURL", "AdaptationSet"],
    )?;
    if period
        .attribute("start")
        .map(timeline::duration)
        .transpose()?
        .is_some_and(|n| n != 0)
        || period
            .attribute("duration")
            .map(timeline::duration)
            .transpose()?
            .is_some_and(|n| n != duration)
    {
        return Err(ErrorCode::Unsupported);
    }
    let base = base_url(
        root,
        &Url::parse(&fetched.resource.url).map_err(|_| ErrorCode::InvalidArgument)?,
        &fetched.resource,
        build,
    )?;
    let base = base_url(period, &base, &fetched.resource, build)?;
    let mut tracks = Vec::new();
    for adaptation in period
        .children()
        .filter(|n| n.has_tag_name((NS, "AdaptationSet")))
    {
        node(
            adaptation,
            &[
                "id",
                "contentType",
                "mimeType",
                "codecs",
                "lang",
                "width",
                "height",
                "audioSamplingRate",
                "segmentAlignment",
                "subsegmentAlignment",
                "startWithSAP",
                "par",
                "frameRate",
                "maxWidth",
                "maxHeight",
                "maxFrameRate",
                "bitstreamSwitching",
            ],
            &[
                "BaseURL",
                "Representation",
                "SegmentBase",
                "SegmentList",
                "SegmentTemplate",
            ],
        )?;
        for flag in [
            "segmentAlignment",
            "subsegmentAlignment",
            "bitstreamSwitching",
        ] {
            if adaptation
                .attribute(flag)
                .is_some_and(|v| !matches!(v, "true" | "false" | "0" | "1"))
            {
                return Err(ErrorCode::Unsupported);
            }
        }
        let adaptation_base = base_url(adaptation, &base, &fetched.resource, build)?;
        for representation in adaptation
            .children()
            .filter(|n| n.has_tag_name((NS, "Representation")))
        {
            if tracks.len() >= 32 {
                return Err(ErrorCode::QuotaExceeded);
            }
            node(
                representation,
                &[
                    "id",
                    "mimeType",
                    "codecs",
                    "bandwidth",
                    "width",
                    "height",
                    "audioSamplingRate",
                    "frameRate",
                    "sar",
                    "startWithSAP",
                ],
                &[
                    "BaseURL",
                    "SegmentBase",
                    "SegmentList",
                    "SegmentTemplate",
                    "AudioChannelConfiguration",
                ],
            )?;
            if let Some(channels) = one(representation, "AudioChannelConfiguration")? {
                node(channels, &["schemeIdUri", "value"], &[])?;
                if need(channels, "schemeIdUri")?
                    != "urn:mpeg:dash:23003:3:audio_channel_configuration:2011"
                    || !(1..=32).contains(&integer(need(channels, "value")?)?)
                {
                    return Err(ErrorCode::Unsupported);
                }
                // The selected sample entry describes the channel layout. This
                // descriptor is validated and discarded, never dereferenced.
            }
            let representation_base =
                base_url(representation, &adaptation_base, &fetched.resource, build)?;
            let id = need(representation, "id")?;
            // Template substitutions are restricted identifiers; delimiters may
            // not be introduced by RepresentationID.
            manifest::logical(id)?;
            let get = |name| {
                representation
                    .attribute(name)
                    .or_else(|| adaptation.attribute(name))
            };
            let mime = get("mimeType").ok_or(ErrorCode::InvalidArgument)?;
            let kind = if mime.starts_with("video/") {
                TrackKind::Video
            } else if mime.starts_with("audio/") {
                TrackKind::Audio
            } else if matches!(mime, "text/vtt" | "application/mp4") {
                TrackKind::Subtitle
            } else {
                return Err(ErrorCode::Unsupported);
            };
            let bandwidth = decimal(need(representation, "bandwidth")?)?;
            let addressing = addressing(representation)?
                .or(addressing(adaptation)?)
                .ok_or(ErrorCode::Unsupported)?;
            let timescale = addressing
                .attribute("timescale")
                .map(decimal)
                .transpose()?
                .unwrap_or(1);
            let timescale = u32::try_from(timescale).map_err(|_| ErrorCode::InvalidArgument)?;
            if timescale == 0 || timescale > 1_000_000_000 {
                return Err(ErrorCode::InvalidArgument);
            }
            let offset = addressing
                .attribute("presentationTimeOffset")
                .map(decimal)
                .transpose()?
                .unwrap_or(0);
            let (init, segments) = segments(
                addressing,
                &fetched.resource,
                &representation_base,
                id,
                bandwidth,
                duration,
                timescale,
                offset,
                kind,
                build,
            )?;
            tracks.push(DashTrack {
                id: stable_id("track", &format!("{}:{id}", fetched.resource.url)),
                kind,
                codec: get("codecs")
                    .unwrap_or(if mime == "text/vtt" { "wvtt" } else { "" })
                    .into(),
                mime_type: mime.into(),
                bandwidth: Id(bandwidth),
                width: get("width").map(integer).transpose()?,
                height: get("height").map(integer).transpose()?,
                sample_rate: get("audioSamplingRate").map(integer).transpose()?,
                lang: adaptation.attribute("lang").map(str::to_owned),
                timescale,
                presentation_time_offset: Id(offset),
                init,
                segments,
            });
        }
    }
    if tracks.is_empty() {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(VirtualDash {
        resources: std::mem::take(&mut build.resources).into_values().collect(),
        duration_ms: Id(duration),
        tracks,
    })
}
#[allow(clippy::too_many_arguments)]
fn segments(
    address: Node<'_, '_>,
    parent: &RemoteResource,
    base: &Url,
    id: &str,
    bandwidth: u64,
    duration: u64,
    timescale: u32,
    offset: u64,
    kind: TrackKind,
    build: &mut Build<'_>,
) -> Result<(Option<ResourceSpan>, DashAddressing), ErrorCode> {
    let resource_kind = if kind == TrackKind::Subtitle {
        ResourceKind::Subtitle
    } else {
        ResourceKind::Media
    };
    let ext = if kind == TrackKind::Subtitle {
        ResourceExtension::Vtt
    } else {
        ResourceExtension::M4s
    };
    let mut init = None;
    if let Some(initialization) = one(address, "Initialization")? {
        node(initialization, &["sourceURL", "range"], &[])?;
        let resource_id = build.bind(
            parent,
            base,
            initialization
                .attribute("sourceURL")
                .unwrap_or(base.as_str()),
            ResourceKind::Init,
            ResourceExtension::Mp4,
        )?;
        init = Some(ResourceSpan {
            resource_id,
            range: initialization
                .attribute("range")
                .map(byte_range)
                .transpose()?,
        });
    }
    let addressing = match address.tag_name().name() {
        "SegmentBase" => {
            node(
                address,
                &[
                    "timescale",
                    "presentationTimeOffset",
                    "indexRange",
                    "indexRangeExact",
                ],
                &["Initialization"],
            )?;
            if address
                .attribute("indexRangeExact")
                .is_some_and(|s| !matches!(s, "true" | "false" | "0" | "1"))
            {
                return Err(ErrorCode::InvalidArgument);
            }
            let index = byte_range(need(address, "indexRange")?)?;
            let resource_id = build.bind(
                parent,
                base,
                base.as_str(),
                ResourceKind::Media,
                ResourceExtension::Mp4,
            )?;
            DashAddressing::SegmentBase {
                media: ResourceSpan {
                    resource_id,
                    range: None,
                },
                index,
            }
        }
        "SegmentList" => {
            node(
                address,
                &[
                    "timescale",
                    "presentationTimeOffset",
                    "duration",
                    "startNumber",
                ],
                &["Initialization", "SegmentTimeline", "SegmentURL"],
            )?;
            let urls: Vec<_> = address
                .children()
                .filter(|n| n.has_tag_name((NS, "SegmentURL")))
                .collect();
            if urls.is_empty() || urls.len() > manifest::RESOURCE_LIMIT {
                return Err(ErrorCode::QuotaExceeded);
            }
            let timing = timeline::expand(address, duration, timescale, offset, Some(urls.len()))?;
            if timing.len() != urls.len() {
                return Err(ErrorCode::InvalidArgument);
            }
            let mut segments = Vec::new();
            for (url, (time, duration)) in urls.into_iter().zip(timing) {
                node(url, &["media", "mediaRange"], &[])?;
                let resource_id =
                    build.bind(parent, base, need(url, "media")?, resource_kind, ext)?;
                segments.push(DashSegment {
                    resource: ResourceSpan {
                        resource_id,
                        range: url.attribute("mediaRange").map(byte_range).transpose()?,
                    },
                    time: Id(time),
                    duration: Id(duration),
                });
            }
            DashAddressing::SegmentList { segments }
        }
        "SegmentTemplate" => {
            node(
                address,
                &[
                    "timescale",
                    "presentationTimeOffset",
                    "duration",
                    "startNumber",
                    "media",
                    "initialization",
                ],
                &["SegmentTimeline"],
            )?;
            let start = address
                .attribute("startNumber")
                .map(decimal)
                .transpose()?
                .unwrap_or(1);
            let media = need(address, "media")?;
            if let Some(template) = address.attribute("initialization") {
                let uri = timeline::template(template, id, bandwidth, None, None)?;
                let resource_id = build.bind(
                    parent,
                    base,
                    &uri,
                    ResourceKind::Init,
                    ResourceExtension::Mp4,
                )?;
                init = Some(ResourceSpan {
                    resource_id,
                    range: None,
                });
            }
            let timing = timeline::expand(address, duration, timescale, offset, None)?;
            let mut segments = Vec::new();
            for (index, (time, duration)) in timing.into_iter().enumerate() {
                let number = start
                    .checked_add(index as u64)
                    .ok_or(ErrorCode::RangeInvalid)?;
                let uri = timeline::template(media, id, bandwidth, Some(number), Some(time))?;
                let resource_id = build.bind(parent, base, &uri, resource_kind, ext)?;
                segments.push(DashSegment {
                    resource: ResourceSpan {
                        resource_id,
                        range: None,
                    },
                    time: Id(time),
                    duration: Id(duration),
                });
            }
            DashAddressing::SegmentList { segments }
        }
        _ => return Err(ErrorCode::Unsupported),
    };
    Ok((init, addressing))
}
fn base_url(
    node_: Node<'_, '_>,
    base: &Url,
    parent: &RemoteResource,
    build: &Build<'_>,
) -> Result<Url, ErrorCode> {
    if let Some(child) = one(node_, "BaseURL")? {
        node(child, &[], &[])?;
        let url = build
            .factory
            .reference(
                parent,
                base,
                child.text().ok_or(ErrorCode::InvalidArgument)?,
            )?
            .url;
        Url::parse(&url).map_err(|_| ErrorCode::InvalidArgument)
    } else {
        Ok(base.clone())
    }
}
fn addressing<'a, 'b>(node_: Node<'a, 'b>) -> Result<Option<Node<'a, 'b>>, ErrorCode> {
    let mut found = None;
    for child in node_.children().filter(Node::is_element) {
        if matches!(
            child.tag_name().name(),
            "SegmentBase" | "SegmentList" | "SegmentTemplate"
        ) {
            if found.is_some() {
                return Err(ErrorCode::InvalidArgument);
            }
            found = Some(child);
        }
    }
    Ok(found)
}
fn node(node: Node<'_, '_>, attrs: &[&str], children: &[&str]) -> Result<(), ErrorCode> {
    if node.tag_name().namespace() != Some(NS)
        || node.attributes().any(|a| {
            let schema = node.has_tag_name((NS, "MPD"))
                && a.name() == "schemaLocation"
                && a.namespace() == Some("http://www.w3.org/2001/XMLSchema-instance");
            (!schema && (a.namespace().is_some() || !attrs.contains(&a.name())))
                || a.value().len() > 4096
        })
        || node.children().filter(Node::is_element).any(|n| {
            n.tag_name().namespace() != Some(NS) || !children.contains(&n.tag_name().name())
        })
    {
        return Err(ErrorCode::Unsupported);
    }
    Ok(())
}
fn one<'a, 'b>(node: Node<'a, 'b>, name: &str) -> Result<Option<Node<'a, 'b>>, ErrorCode> {
    let mut iter = node.children().filter(|n| n.has_tag_name((NS, name)));
    let first = iter.next();
    if iter.next().is_some() {
        return Err(ErrorCode::Unsupported);
    }
    Ok(first)
}
fn need<'a, 'b>(node: Node<'a, 'b>, name: &str) -> Result<&'a str, ErrorCode> {
    node.attribute(name).ok_or(ErrorCode::InvalidArgument)
}
fn integer(value: &str) -> Result<u32, ErrorCode> {
    u32::try_from(decimal(value)?).map_err(|_| ErrorCode::InvalidArgument)
}
fn byte_range(value: &str) -> Result<ByteRange, ErrorCode> {
    let (start, end) = value.split_once('-').ok_or(ErrorCode::RangeInvalid)?;
    let start = decimal(start)?;
    let end = decimal(end)?;
    Ok(ByteRange {
        offset: Id(start),
        len: Id(end
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or(ErrorCode::RangeInvalid)?),
    })
}
