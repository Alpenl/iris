//! No input XML or HLS line is passed through to a demuxer. Rendering uses an
//! explicit format profile and URLs supplied solely by the session registry.
mod dash;
mod hls;
pub(crate) mod restricted;
use iris_api::v2::{ErrorCode, stream::*};
use std::collections::{BTreeMap, BTreeSet};

pub const RESOURCE_LIMIT: usize = 10_000;
pub const INPUT_LIMIT: usize = 4 << 20;
pub const OUTPUT_LIMIT: usize = 16 << 20;
pub const ROOT: &str = "__iris_root";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Hls,
    Dash,
}
pub struct Document {
    pub id: String,
    pub bytes: Vec<u8>,
    pub profile: Profile,
}
pub struct Manifest {
    pub resources: BTreeMap<String, ResourceBinding>,
    pub documents: Vec<Document>,
    pub profile: Profile,
}

/// `local` receives only validated logical identifiers, including host-generated
/// document IDs. It must return an absolute loopback URL in this session.
pub fn compile(
    format: ManifestFormat,
    bytes: &[u8],
    local: &dyn Fn(&str, ResourceExtension) -> Result<String, ErrorCode>,
) -> Result<Manifest, ErrorCode> {
    if bytes.len() > INPUT_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut build = match format {
        ManifestFormat::VirtualHls => {
            let input: VirtualHls = crate::plugin_v2::json::decode_exact(bytes, INPUT_LIMIT)?;
            let mut build = Build::new(input.resources.clone(), Profile::Hls, local)?;
            hls::render(input, &mut build)?;
            build
        }
        ManifestFormat::VirtualDash => {
            let input: VirtualDash = crate::plugin_v2::json::decode_exact(bytes, INPUT_LIMIT)?;
            let mut build = Build::new(input.resources.clone(), Profile::Dash, local)?;
            dash::render(input, &mut build)?;
            build
        }
        ManifestFormat::RestrictedHls => {
            let input: RestrictedHls = crate::plugin_v2::json::decode_exact(bytes, INPUT_LIMIT)?;
            let mut build = Build::new(input.resources, Profile::Hls, local)?;
            restricted::render(&input.text, &mut build)?;
            build
        }
    };
    if build.documents.is_empty() || !build.documents.iter().any(|d| d.id == ROOT) {
        return Err(ErrorCode::InvalidArgument);
    }
    // Reject unused capabilities; a manifest cannot smuggle an unrelated
    // fetchable resource into the session just by listing it.
    build.resources.retain(|id, _| build.used.contains(id));
    Ok(Manifest {
        resources: build.resources,
        documents: build.documents,
        profile: build.profile,
    })
}

struct Build<'a> {
    resources: BTreeMap<String, ResourceBinding>,
    documents: Vec<Document>,
    used: BTreeSet<String>,
    output: usize,
    profile: Profile,
    local: &'a dyn Fn(&str, ResourceExtension) -> Result<String, ErrorCode>,
}
impl<'a> Build<'a> {
    fn new(
        resources: Vec<ResourceBinding>,
        profile: Profile,
        local: &'a dyn Fn(&str, ResourceExtension) -> Result<String, ErrorCode>,
    ) -> Result<Self, ErrorCode> {
        if resources.len() > RESOURCE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut map = BTreeMap::new();
        for resource in resources {
            logical(&resource.id)?;
            if let ResourceSource::Guest { resource_id } = &resource.source {
                logical(resource_id)?;
            }
            if resource.kind == ResourceKind::Manifest {
                // Nested remote manifests are handled by the bounded remote
                // parser, never by treating opaque bytes as a segment.
                return Err(ErrorCode::Unsupported);
            }
            let allowed = match resource.kind {
                ResourceKind::Key => {
                    resource.extension == ResourceExtension::Key
                        && resource.size.is_none_or(|n| n.0 == 16)
                }
                ResourceKind::Subtitle => matches!(
                    resource.extension,
                    ResourceExtension::Vtt | ResourceExtension::Srt | ResourceExtension::Ass
                ),
                _ => matches!(
                    resource.extension,
                    ResourceExtension::Mp4
                        | ResourceExtension::M4a
                        | ResourceExtension::M4s
                        | ResourceExtension::Ts
                        | ResourceExtension::Aac
                        | ResourceExtension::Webm
                ),
            };
            if !allowed || map.insert(resource.id.clone(), resource).is_some() {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        Ok(Self {
            resources: map,
            documents: Vec::new(),
            used: BTreeSet::new(),
            output: 0,
            profile,
            local,
        })
    }
    fn resource(&mut self, id: &str, kinds: &[ResourceKind]) -> Result<String, ErrorCode> {
        let resource = self.resources.get(id).ok_or(ErrorCode::InvalidArgument)?;
        if !kinds.contains(&resource.kind) {
            return Err(ErrorCode::InvalidArgument);
        }
        self.used.insert(id.to_owned());
        (self.local)(id, resource.extension)
    }
    fn span(&mut self, span: &ResourceSpan, kinds: &[ResourceKind]) -> Result<String, ErrorCode> {
        let url = self.resource(&span.resource_id, kinds)?;
        if let Some(range) = &span.range {
            let end = range_end(range)?;
            if self.resources[&span.resource_id]
                .size
                .is_some_and(|n| end > n.0)
            {
                return Err(ErrorCode::RangeInvalid);
            }
        }
        Ok(url)
    }
    fn add(&mut self, id: &str, text: String) -> Result<(), ErrorCode> {
        self.output = self
            .output
            .checked_add(text.len())
            .ok_or(ErrorCode::QuotaExceeded)?;
        if self.output > OUTPUT_LIMIT
            || self.resources.len() + self.documents.len() >= RESOURCE_LIMIT
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        self.documents.push(Document {
            id: id.into(),
            bytes: text.into_bytes(),
            profile: self.profile,
        });
        Ok(())
    }
    fn document_url(&self, id: &str) -> Result<String, ErrorCode> {
        (self.local)(
            id,
            match self.profile {
                Profile::Hls => ResourceExtension::M3u8,
                Profile::Dash => ResourceExtension::Mpd,
            },
        )
    }
}

pub(crate) fn logical(value: &str) -> Result<(), ErrorCode> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with("__iris_")
        || value.contains("..")
        || value == "."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_- .".contains(&b) && b != b' ')
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(())
}
fn attribute(value: &str) -> Result<&str, ErrorCode> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '<' | '>' | '&'))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(value)
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
fn append(out: &mut String, value: &str) -> Result<(), ErrorCode> {
    if out.len().saturating_add(value.len()) > OUTPUT_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    out.push_str(value);
    Ok(())
}

#[cfg(test)]
mod tests;
