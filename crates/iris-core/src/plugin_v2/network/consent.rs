//! Host-owned Origin consent. DNS resolution produces a review only; it never
//! grants access. Persisted pins cannot be modified through Guest state APIs.
use super::{
    AddressKind, NetworkGrant, Origin, TransportKind, address_kind, grant::validate_pins,
    normalize_url,
};
use crate::{
    fs_util,
    plugin_v2::{
        installation::valid_identity,
        ledger::{Ledger, Reservation, Resource},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, Id, task_result::OriginResult};
use crate::plugin_v2::json;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{ErrorKind, Read},
    net::{IpAddr, ToSocketAddrs},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

const MAX_RECORD: usize = 64 << 10;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedOrigin {
    pub origin: String,
    pub ips: Vec<IpAddr>,
    pub insecure_auth: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    installation: String,
    entries: BTreeMap<String, SavedOrigin>,
}
#[derive(Clone)]
pub struct OriginStore {
    owner: String,
    path: PathBuf,
    ledger: Ledger,
}
pub struct OriginReview {
    owner: String,
    version: Id,
    origin: String,
    ips: Vec<IpAddr>,
    previous: Option<SavedOrigin>,
    expires: Instant,
    _memory: Reservation,
}
impl OriginReview {
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn ips(&self) -> &[IpAddr] {
        &self.ips
    }
    pub fn previous(&self) -> Option<&SavedOrigin> {
        self.previous.as_ref()
    }
    pub fn private(&self) -> bool {
        self.ips
            .iter()
            .any(|ip| address_kind(*ip) != AddressKind::Public)
    }
    pub fn plaintext(&self) -> bool {
        self.origin.starts_with("http:")
    }
}
impl OriginStore {
    pub fn new(root: PathBuf, owner: String, ledger: Ledger) -> Result<Self, ErrorCode> {
        if !valid_identity(&owner) {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Self {
            path: root.join("origins").join(format!("{owner}.json")),
            owner,
            ledger,
        })
    }
    fn read(&self) -> Result<Record, ErrorCode> {
        let file = match fs_util::retry_sharing_violation(|| std::fs::File::open(&self.path)) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Record {
                    schema: 1,
                    installation: self.owner.clone(),
                    entries: BTreeMap::new(),
                });
            }
            Err(_) => return Err(ErrorCode::IoError),
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ErrorCode::IoError)?;
        let record: Record = json::decode(&bytes, MAX_RECORD)?;
        if record.schema != 1 || record.installation != self.owner || record.entries.len() > 16 {
            return Err(ErrorCode::InvalidArgument);
        }
        for (key, entry) in &record.entries {
            let (origin, pins, _) = validate_pins(&entry.origin, TransportKind::Http, &entry.ips)?;
            if origin.display() != *key
                || entry.origin != *key
                || pins.len() != entry.ips.len()
                || (entry.insecure_auth && origin.scheme != "http")
            {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        Ok(record)
    }
    fn check(&self, grant: &NetworkGrant, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        grant.check_live()?;
        if grant.owner() != self.owner {
            return Err(ErrorCode::Denied);
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(())
    }
    /// Startup worker only. Restore exact persisted pins without re-resolving or
    /// widening them; a changed private address requires another user review.
    pub fn restore(&self, grant: &NetworkGrant, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        self.check(grant, cancel)?;
        let _memory = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, 2 << 20)])?;
        let record = self.read()?;
        for entry in record.entries.values() {
            self.check(grant, cancel)?;
            let id = grant.confirm_origin(&entry.origin, TransportKind::Http, &entry.ips)?;
            if entry.insecure_auth {
                grant.confirm_insecure_auth(id)?;
            }
        }
        Ok(())
    }
    /// Background-only; the returned object is private immutable review data.
    pub fn review(
        &self,
        grant: &NetworkGrant,
        raw: &str,
        cancel: &HttpCancel,
    ) -> Result<OriginReview, ErrorCode> {
        self.check(grant, cancel)?;
        let memory = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, 2 << 20)])?;
        let origin = parse_consent_origin(raw)?;
        let ips = resolve(&origin, &self.owner, &self.ledger, cancel)?;
        validate_pins(&origin.display(), TransportKind::Http, &ips)?;
        let previous = self.read()?.entries.remove(&origin.display());
        self.check(grant, cancel)?;
        Ok(OriginReview {
            owner: self.owner.clone(),
            version: grant.version(),
            origin: origin.display(),
            ips,
            previous,
            expires: Instant::now() + Duration::from_secs(300),
            _memory: memory,
        })
    }
    /// The trusted host supplies the separate plaintext-credential checkbox.
    /// The install/Origin identity and exact previously displayed pins are CAS
    /// checked under the file lock; another window cannot silently replace them.
    pub fn approve(
        &self,
        grant: &NetworkGrant,
        review: OriginReview,
        insecure_auth: bool,
        cancel: &HttpCancel,
    ) -> Result<OriginResult, ErrorCode> {
        self.check(grant, cancel)?;
        if review.owner != self.owner
            || review.version != grant.version()
            || Instant::now() >= review.expires
        {
            return Err(ErrorCode::StaleHandle);
        }
        if insecure_auth && !review.plaintext() {
            return Err(ErrorCode::InvalidArgument);
        }
        let _lock = fs_util::lock_file_with_timeout(
            &self.path.with_extension("lock"),
            Duration::from_millis(250),
        )
        .map_err(|e| {
            if e.kind() == ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let mut record = self.read()?;
        if record.entries.get(&review.origin) != review.previous.as_ref() {
            return Err(ErrorCode::StaleHandle);
        }
        if record.entries.len() >= 16 && !record.entries.contains_key(&review.origin) {
            return Err(ErrorCode::QuotaExceeded);
        }
        record.entries.insert(
            review.origin.clone(),
            SavedOrigin {
                origin: review.origin.clone(),
                ips: review.ips.clone(),
                insecure_auth,
            },
        );
        let bytes = json::encode(&record, MAX_RECORD)?;
        self.check(grant, cancel)?;
        fs_util::atomic_write_private(&self.path, &bytes).map_err(|_| ErrorCode::IoError)?;
        // Persistence can win a race with cancellation. A later reload observes
        // exactly this explicit approval; it never fabricates an empty record.
        self.check(grant, cancel)?;
        let id = grant.confirm_origin(&review.origin, TransportKind::Http, &review.ips)?;
        if insecure_auth {
            grant.confirm_insecure_auth(id)?;
        }
        grant.lookup_origin(&review.origin, TransportKind::Http)
    }
    pub fn revoke(
        &self,
        grant: &NetworkGrant,
        origin: &str,
        cancel: &HttpCancel,
    ) -> Result<(), ErrorCode> {
        self.check(grant, cancel)?;
        let origin = parse_consent_origin(origin)?.display();
        let _lock = fs_util::lock_file_with_timeout(
            &self.path.with_extension("lock"),
            Duration::from_millis(250),
        )
        .map_err(|_| ErrorCode::Backpressure)?;
        let mut record = self.read()?;
        record.entries.remove(&origin);
        self.check(grant, cancel)?;
        fs_util::atomic_write_private(&self.path, &json::encode(&record, MAX_RECORD)?)
            .map_err(|_| ErrorCode::IoError)?;
        if let Ok(found) = grant.lookup_origin(&origin, TransportKind::Http) {
            grant.revoke_origin(found.origin_grant)?;
        }
        Ok(())
    }
}
fn parse_consent_origin(raw: &str) -> Result<Origin, ErrorCode> {
    let url = normalize_url(raw, TransportKind::Http)
        .or_else(|_| normalize_url(raw, TransportKind::WebSocket))?;
    Ok(Origin::of(&url)?.http_equivalent())
}
fn resolve(
    origin: &Origin,
    owner: &str,
    ledger: &Ledger,
    cancel: &HttpCancel,
) -> Result<Vec<IpAddr>, ErrorCode> {
    if let Ok(ip) = origin.host.parse::<IpAddr>() {
        return Ok(vec![ip.to_canonical()]);
    }
    let reservation = ledger.reserve(owner, &[(Resource::Dns, 1), (Resource::HostBytes, 8192)])?;
    let host = origin.host.clone();
    let job_cancel = cancel.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("iris-origin-dns".into())
        .spawn(move || {
            let _reservation = reservation;
            let result = (host.as_str(), 0)
                .to_socket_addrs()
                .map_err(|_| ErrorCode::IoError)
                .and_then(|addrs| {
                    let addresses: Vec<_> = addrs.take(65).collect();
                    if addresses.len() > 64 {
                        return Err(ErrorCode::QuotaExceeded);
                    }
                    let mut ips: BTreeSet<_> = addresses
                        .iter()
                        .map(|a| a.ip().to_canonical())
                        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
                        .collect();
                    let snapshot: Vec<_> = ips.iter().copied().collect();
                    if crate::vfs::http_client::doh::should_bypass_fake_ip(&host, &snapshot) {
                        let real = crate::vfs::http_client::doh::resolve_a_records(
                            &host,
                            &job_cancel,
                            Instant::now() + Duration::from_secs(8),
                        )
                        .map_err(|_| ErrorCode::IoError)?;
                        ips = real.into_iter().map(|ip| ip.to_canonical()).collect();
                    }
                    if ips.is_empty() {
                        return Err(ErrorCode::Denied);
                    }
                    if ips.len() > 32 {
                        return Err(ErrorCode::QuotaExceeded);
                    }
                    Ok(ips.into_iter().collect())
                });
            let _ = tx.send(result);
        })
        .map_err(|_| ErrorCode::Internal)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ErrorCode::Timeout)?;
        match rx.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err(ErrorCode::Internal),
        }
    }
}

#[cfg(test)]
mod tests;
