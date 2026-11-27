//! Remote HLS playlists are converted to the same validated structured model as
//! guest manifests. Network URLs remain only in authorized resource bindings.
mod hls;
mod hls_parse;
pub mod live;
use super::{
    manifest,
    remote::{Fetched, RemoteFactory},
};
use crate::ledger::Resource;
pub use hls_parse::{attributes as parse_hls_attributes, parse_media as parse_hls_media};
use iris_api::v2::{ErrorCode, stream::*};
use iris_net::http_client::HttpCancel;
use std::collections::BTreeMap;
use url::Url;

pub struct Converted {
    pub format: ManifestFormat,
    pub bytes: Vec<u8>,
    pub live: bool,
    pub refresh_after: std::time::Duration,
    _memory: crate::ledger::Reservation,
}
pub fn convert(
    factory: &RemoteFactory,
    resource: &RemoteResource,
    cancel: &HttpCancel,
) -> Result<Converted, ErrorCode> {
    let _scratch = factory
        .ledger()
        .reserve(factory.owner(), &[(Resource::HostBytes, 32 << 20)])?;
    let fetched = factory.fetch(resource, cancel, manifest::INPUT_LIMIT)?;
    let mut build = Build {
        factory,
        cancel,
        resources: BTreeMap::new(),
        by_url: BTreeMap::new(),
        cost: 0,
        downloaded: fetched.bytes.len(),
    };
    let text = std::str::from_utf8(&fetched.bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?
        .trim_start_matches('\u{feff}');
    if !text.starts_with("#EXTM3U") {
        return Err(ErrorCode::Unsupported);
    }
    let document = hls::parse(&fetched, &mut build)?;
    let live = document.live;
    let target = document
        .tracks
        .iter()
        .filter_map(|t| t.segments.iter().map(|s| s.duration_ms.0).max())
        .min()
        .unwrap_or(2000);
    let format = ManifestFormat::VirtualHls;
    let bytes = iris_core::bounded_json::encode(&document, manifest::INPUT_LIMIT)
        .map_err(crate::json_error)?;
    let interval = (target / 2).clamp(250, 5000);
    let memory = factory.ledger().reserve(
        factory.owner(),
        &[(Resource::HostBytes, bytes.len() as u64)],
    )?;
    Ok(Converted {
        format,
        bytes,
        live,
        refresh_after: std::time::Duration::from_millis(interval),
        _memory: memory,
    })
}
struct Build<'a> {
    factory: &'a RemoteFactory,
    cancel: &'a HttpCancel,
    resources: BTreeMap<String, ResourceBinding>,
    by_url: BTreeMap<(String, String), String>,
    cost: usize,
    downloaded: usize,
}
impl Build<'_> {
    fn bind(
        &mut self,
        parent: &RemoteResource,
        base: &Url,
        reference: &str,
        kind: ResourceKind,
        extension: ResourceExtension,
    ) -> Result<String, ErrorCode> {
        let resource = self.factory.reference(parent, base, reference)?;
        let key = (format!("{kind:?}:{extension:?}"), resource.url.clone());
        if let Some(id) = self.by_url.get(&key) {
            if self.resources[id].source != (ResourceSource::Remote { request: resource }) {
                return Err(ErrorCode::Denied);
            }
            return Ok(id.clone());
        }
        let cost = 1024
            + resource.url.len() * 2
            + resource
                .headers
                .iter()
                .map(|(k, v)| (k.len() + v.len()) * 2)
                .sum::<usize>();
        self.cost = self
            .cost
            .checked_add(cost)
            .ok_or(ErrorCode::QuotaExceeded)?;
        if self.cost > 16 << 20 || self.resources.len() >= manifest::RESOURCE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let id = stable_id("r", &format!("{}:{}", key.0, key.1));
        if self.resources.contains_key(&id) {
            return Err(ErrorCode::Internal);
        }
        self.resources.insert(
            id.clone(),
            ResourceBinding {
                id: id.clone(),
                kind,
                extension,
                size: if kind == ResourceKind::Key {
                    Some(iris_api::v2::WireU64(16))
                } else {
                    None
                },
                source: ResourceSource::Remote { request: resource },
            },
        );
        self.by_url.insert(key, id.clone());
        Ok(id)
    }
    fn child(&mut self, parent: &RemoteResource, reference: &str) -> Result<Fetched, ErrorCode> {
        let resource = self.factory.reference(
            parent,
            &Url::parse(&parent.url).map_err(|_| ErrorCode::InvalidArgument)?,
            reference,
        )?;
        let remaining = manifest::INPUT_LIMIT
            .checked_sub(self.downloaded)
            .ok_or(ErrorCode::QuotaExceeded)?;
        let fetched = self.factory.fetch(&resource, self.cancel, remaining)?;
        self.downloaded += fetched.bytes.len();
        Ok(fetched)
    }
}
fn stable_id(prefix: &str, value: &str) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, value.as_bytes());
    let mut id = format!("{prefix}_");
    for b in &hash.as_ref()[..16] {
        use std::fmt::Write;
        let _ = write!(id, "{b:02x}");
    }
    id
}

#[cfg(test)]
mod tests;
