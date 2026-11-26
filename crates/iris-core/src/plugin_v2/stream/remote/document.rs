use super::{RemoteFactory, identity_encoding, unique};
use crate::{
    plugin_v2::{
        ledger::{Reservation, Resource},
        network::{Origin, TransportKind, normalize_url, raw::RawState},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, stream::RemoteResource};
use std::time::{Duration, Instant};
use url::Url;
pub(crate) struct Fetched {
    pub resource: RemoteResource,
    pub bytes: Vec<u8>,
    _memory: Reservation,
}
pub struct Thumbnail {
    data: Fetched,
}
impl Thumbnail {
    pub fn bytes(&self) -> &[u8] {
        &self.data.bytes
    }
}
impl RemoteFactory {
    /// Background host image fetch. The same Origin/DNS/redirect/credential
    /// policy as playback is enforced; the caller must separately bound decode.
    pub fn fetch_thumbnail(
        &self,
        resource: &RemoteResource,
        cancel: &HttpCancel,
    ) -> Result<Thumbnail, ErrorCode> {
        self.fetch(resource, cancel, 4 << 20)
            .map(|data| Thumbnail { data })
    }
    pub fn reference(
        &self,
        parent: &RemoteResource,
        base: &Url,
        reference: &str,
    ) -> Result<RemoteResource, ErrorCode> {
        if reference.is_empty()
            || reference.len() > 4096
            || reference.chars().any(|c| c.is_control() || c == '\\')
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let target = base
            .join(reference)
            .map_err(|_| ErrorCode::InvalidArgument)?;
        let target = normalize_url(target.as_str(), TransportKind::Http)?;
        let same =
            Origin::of(&normalize_url(&parent.url, TransportKind::Http)?)? == Origin::of(&target)?;
        let resource = RemoteResource {
            url: target.to_string(),
            headers: parent
                .headers
                .iter()
                .filter(|(k, _)| {
                    same || ["accept", "user-agent"]
                        .iter()
                        .any(|s| k.eq_ignore_ascii_case(s))
                })
                .cloned()
                .collect(),
            origin_grant: if same { parent.origin_grant } else { None },
        };
        let grant =
            self.grant
                .request(&resource.url, TransportKind::Http, resource.origin_grant)?;
        grant.check_credentials(&target, &resource.headers)?;
        Ok(resource)
    }
    pub(crate) fn fetch(
        &self,
        resource: &RemoteResource,
        cancel: &HttpCancel,
        limit: usize,
    ) -> Result<Fetched, ErrorCode> {
        if limit == 0 || limit > 16 << 20 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let memory = self.ledger.reserve(
            self.owner(),
            &[(Resource::HostBytes, (limit * 2 + 65536) as u64)],
        )?;
        let reader = self.reader(resource, None)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut response =
            reader
                .target
                .open_range("GET", None, false, 2 << 20, cancel, deadline)?;
        let RawState::Response(raw) = &response.state else {
            return Err(ErrorCode::Internal);
        };
        if raw.status != 200 {
            return Err(if matches!(raw.status, 401 | 403) {
                ErrorCode::AuthFailed
            } else {
                ErrorCode::IoError
            });
        }
        identity_encoding(&raw.headers)?;
        if unique(&raw.headers, "content-length")?
            .map(super::super::range::decimal)
            .transpose()?
            .is_some_and(|n| n > limit as u64)
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        let final_resource = self.reference(resource, &raw.url, raw.url.as_str())?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 16384];
        loop {
            let n = response.state.read(&mut chunk, deadline)?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > limit {
                return Err(ErrorCode::QuotaExceeded);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        Ok(Fetched {
            resource: final_resource,
            bytes,
            _memory: memory,
        })
    }
}
