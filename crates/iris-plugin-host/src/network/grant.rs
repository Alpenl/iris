use super::{AddressKind, address_kind};
use crate::instance::next_id;
use crate::ledger::{Activity, ActivityLease, Ledger, Reservation, Resource};
use crate::url_resolver::{
    Origin, TransportKind, matches_host, normalize_url, validate_declaration,
};
use iris_api::v2::{Capability, ErrorCode, Id, NetworkDeclaration};
use iris_base::net::http_client::HttpCancel;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use url::{Host, Url};

struct ConfirmedOrigin {
    origin: Origin,
    ips: BTreeSet<IpAddr>,
    active: AtomicBool,
    cancel: HttpCancel,
    insecure_auth: AtomicBool,
    cookies: Arc<super::cookies::CookieJar>,
}
struct Granted {
    owner: String,
    version: u64,
    capabilities: BTreeSet<Capability>,
    declaration: NetworkDeclaration,
    origins: Mutex<BTreeMap<u64, Arc<ConfirmedOrigin>>>,
    active: AtomicBool,
    cancel: HttpCancel,
    _memory: Reservation,
    ledger: Ledger,
}

#[derive(Clone)]
pub struct NetworkGrant(Arc<Granted>);

impl NetworkGrant {
    /// Call only with capabilities confirmed by the host, not the raw manifest.
    pub fn new(
        owner: String,
        capabilities: BTreeSet<Capability>,
        mut declaration: NetworkDeclaration,
        ledger: &Ledger,
    ) -> Result<Self, ErrorCode> {
        validate_declaration(&mut declaration)?;
        let memory = ledger.reserve(&owner, &[(Resource::HostBytes, 2 << 20)])?;
        Ok(Self(Arc::new(Granted {
            owner,
            version: next_id()?,
            capabilities,
            declaration,
            origins: Mutex::new(BTreeMap::new()),
            active: AtomicBool::new(true),
            cancel: HttpCancel::default(),
            _memory: memory,
            ledger: ledger.clone(),
        })))
    }

    pub fn owner(&self) -> &str {
        &self.0.owner
    }
    pub fn version(&self) -> Id {
        Id(self.0.version)
    }
    /// Canonical bucket identity, available only for a still-active host grant.
    /// No request URL can mint an Origin credential scope on its own.
    pub(crate) fn confirmed_origin(&self, id: Id) -> Result<Origin, ErrorCode> {
        self.check_live()?;
        self.0
            .origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.0)
            .filter(|origin| origin.active.load(Ordering::Acquire))
            .map(|origin| origin.origin.clone())
            .ok_or(ErrorCode::StaleHandle)
    }
    pub fn validate_origins(&self, ids: &BTreeSet<Id>) -> Result<(), ErrorCode> {
        self.check_live()?;
        let origins = self.0.origins.lock().unwrap_or_else(|e| e.into_inner());
        if ids.iter().any(|id| {
            origins
                .get(&id.0)
                .is_none_or(|o| !o.active.load(Ordering::Acquire))
        }) {
            return Err(ErrorCode::Denied);
        }
        Ok(())
    }
    pub fn revoke(&self) {
        self.0.active.store(false, Ordering::Release);
        self.0.cancel.cancel();
        for origin in self
            .0
            .origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            origin.active.store(false, Ordering::Release);
            origin.cancel.cancel();
        }
    }

    pub(super) fn has_capability(&self, capability: Capability) -> bool {
        self.0.capabilities.contains(&capability)
    }
    fn allows_origin(&self, kind: TransportKind) -> bool {
        match kind {
            TransportKind::Http => self.0.capabilities.iter().any(|c| {
                matches!(
                    c,
                    Capability::NetHttp | Capability::NetWs | Capability::NetTcp
                )
            }),
            TransportKind::WebSocket => {
                self.has_capability(Capability::NetWs) || self.has_capability(Capability::NetHttp)
            }
            TransportKind::Tcp { .. } => self.has_capability(Capability::NetTcp),
        }
    }

    /// The UI supplies the exact displayed Origin and resolved address set after
    /// confirmation. No discovery/redirect/plugin-form code may call this.
    pub fn confirm_origin(
        &self,
        raw: &str,
        kind: TransportKind,
        ips: &[IpAddr],
    ) -> Result<Id, ErrorCode> {
        self.check_live()?;
        if !self.allows_origin(kind) {
            return Err(ErrorCode::Denied);
        }
        let (origin, ips) = validate_pins(raw, kind, ips)?;
        let origin = origin.http_equivalent();
        let cookies = Arc::new(super::cookies::CookieJar::new(
            &self.0.owner,
            &self.0.ledger,
        )?);
        let mut origins = self.0.origins.lock().unwrap_or_else(|e| e.into_inner());
        origins.retain(|_, origin| origin.active.load(Ordering::Acquire));
        if origins.len() >= 16 && !origins.values().any(|old| old.origin == origin) {
            return Err(ErrorCode::QuotaExceeded);
        }
        let id = next_id()?;
        // Reconfirmation replaces the exact Origin and cancels its old pins;
        // multiple live handles cannot preserve a previously revoked address.
        origins.retain(|_, old| {
            if old.origin == origin {
                old.active.store(false, Ordering::Release);
                old.cancel.cancel();
                false
            } else {
                true
            }
        });
        origins.insert(
            id,
            Arc::new(ConfirmedOrigin {
                origin,
                ips,
                active: AtomicBool::new(true),
                cancel: HttpCancel::default(),
                insecure_auth: AtomicBool::new(false),
                cookies,
            }),
        );
        Ok(Id(id))
    }

    pub fn revoke_origin(&self, id: Id) -> Result<(), ErrorCode> {
        let mut origins = self.0.origins.lock().unwrap_or_else(|e| e.into_inner());
        let origin = origins.remove(&id.0).ok_or(ErrorCode::StaleHandle)?;
        origin.active.store(false, Ordering::Release);
        origin.cancel.cancel();
        Ok(())
    }

    /// A separate trusted-host confirmation for credentials over plaintext.
    pub fn confirm_insecure_auth(&self, id: Id) -> Result<(), ErrorCode> {
        self.check_live()?;
        let origins = self.0.origins.lock().unwrap_or_else(|e| e.into_inner());
        let origin = origins.get(&id.0).ok_or(ErrorCode::StaleHandle)?;
        origin.insecure_auth.store(true, Ordering::Release);
        Ok(())
    }

    pub fn request(
        &self,
        raw: &str,
        kind: TransportKind,
        origin: Option<Id>,
    ) -> Result<Arc<RequestGrant>, ErrorCode> {
        self.request_normalized(raw, kind, origin, Activity::default())
            .map(|(grant, _)| grant)
    }
    pub(crate) fn request_normalized(
        &self,
        raw: &str,
        kind: TransportKind,
        origin: Option<Id>,
        activity: Activity,
    ) -> Result<(Arc<RequestGrant>, Url), ErrorCode> {
        self.check_live()?;
        if !self.0.capabilities.contains(&kind.capability()) {
            return Err(ErrorCode::Denied);
        }
        let url = normalize_url(raw, kind)?;
        let origin = origin
            .map(|id| {
                self.0
                    .origins
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id.0)
                    .cloned()
                    .ok_or(ErrorCode::StaleHandle)
            })
            .transpose()?;
        if origin.as_ref().is_some_and(|grant| {
            !grant.active.load(Ordering::Acquire)
                || Origin::of(&url).map(|o| o.http_equivalent()).as_ref() != Ok(&grant.origin)
        }) {
            return Err(ErrorCode::Denied);
        }
        let cancel = match &origin {
            Some(grant) => grant.cancel.child(),
            None => self.0.cancel.child(),
        }
        .map_err(|_| ErrorCode::Cancelled)?;
        let request = Arc::new(RequestGrant {
            network: self.clone(),
            initial: Origin::of(&url)?,
            kind,
            origin,
            cancel,
            activity,
        });
        request.check_url(&url)?;
        Ok((request, url))
    }
    pub(crate) fn check_live(&self) -> Result<(), ErrorCode> {
        if self.0.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(ErrorCode::Denied)
        }
    }
}

impl NetworkGrant {
    pub fn lookup_origin(
        &self,
        raw: &str,
        kind: TransportKind,
    ) -> Result<iris_api::v2::task_result::OriginResult, ErrorCode> {
        self.check_live()?;
        if !self.allows_origin(kind) {
            return Err(ErrorCode::Denied);
        }
        let origin = Origin::of(&normalize_url(raw, kind)?)?;
        let target = origin.http_equivalent();
        let origins = self.0.origins.lock().unwrap_or_else(|e| e.into_inner());
        let (id, grant) = origins
            .iter()
            .find(|(_, g)| g.origin == target && g.active.load(Ordering::Acquire))
            .ok_or(ErrorCode::Denied)?;
        Ok(iris_api::v2::task_result::OriginResult {
            origin: target.display(),
            origin_grant: Id(*id),
            insecure_auth: grant.insecure_auth.load(Ordering::Acquire),
        })
    }
}

pub struct RequestGrant {
    network: NetworkGrant,
    pub initial: Origin,
    pub kind: TransportKind,
    origin: Option<Arc<ConfirmedOrigin>>,
    cancel: HttpCancel,
    activity: Activity,
}
impl RequestGrant {
    pub(crate) fn cookies(&self) -> Option<Arc<super::cookies::CookieJar>> {
        self.origin.as_ref().map(|o| o.cookies.clone())
    }
    pub(crate) fn check_credentials(
        &self,
        url: &Url,
        headers: &[(String, String)],
    ) -> Result<(), ErrorCode> {
        if matches!(url.scheme(), "https" | "wss") {
            return Ok(());
        }
        let sensitive = headers.iter().any(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "cookie"
            )
        });
        if sensitive
            && !self.origin.as_ref().is_some_and(|origin| {
                Origin::of(url).map(|o| o.http_equivalent()).as_ref() == Ok(&origin.origin)
                    && origin.insecure_auth.load(Ordering::Acquire)
            })
        {
            return Err(ErrorCode::Denied);
        }
        Ok(())
    }
    pub(crate) fn reserve_dns(&self) -> Result<(Reservation, ActivityLease), ErrorCode> {
        self.check_live()?;
        let reservation = self.network.0.ledger.reserve(
            self.owner(),
            &[(Resource::Dns, 1), (Resource::HostBytes, 8192)],
        )?;
        Ok((reservation, self.activity.start()))
    }
    pub(crate) fn background(&self) -> ActivityLease {
        self.activity.start()
    }
    pub fn cancel(&self) -> &HttpCancel {
        &self.cancel
    }
    pub fn owner(&self) -> &str {
        self.network.owner()
    }
    pub fn check_live(&self) -> Result<(), ErrorCode> {
        self.network.check_live()?;
        if self
            .origin
            .as_ref()
            .is_some_and(|o| !o.active.load(Ordering::Acquire))
        {
            return Err(ErrorCode::Denied);
        }
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(())
    }
    pub fn check_url(&self, url: &Url) -> Result<(), ErrorCode> {
        self.check_live()?;
        let url = normalize_url(url.as_str(), self.kind)?;
        let origin = Origin::of(&url)?;
        let http_origin = origin.http_equivalent();
        let declaration = &self.network.0.declaration;
        let confirmed = self
            .origin
            .as_ref()
            .is_some_and(|grant| grant.origin == http_origin);
        let literal = match url.host() {
            Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if !confirmed
            && (literal.is_some_and(|ip| address_kind(ip) != AddressKind::Public)
                || origin.host == "localhost"
                || origin.host.ends_with(".localhost")
                || origin.host.ends_with(".local"))
        {
            return Err(ErrorCode::Denied);
        }
        if let TransportKind::Tcp { tls } = self.kind {
            if confirmed && http_origin.scheme == if tls { "https" } else { "http" } {
                return Ok(());
            }
            let scheme = if tls { "tls" } else { "tcp" };
            return if declaration.tcp_targets.iter().any(|r| {
                r.host == origin.host
                    && r.ports.contains(&origin.port)
                    && r.schemes.iter().any(|s| s == scheme)
            }) {
                Ok(())
            } else {
                Err(ErrorCode::Denied)
            };
        }
        if confirmed {
            return Ok(());
        }
        let public = declaration.net_public
            && (matches!(origin.port, 80 | 443) || declaration.public_ports.contains(&origin.port));
        let host = declaration.net_hosts.iter().any(|rule| {
            matches_host(&rule.host, &origin.host)
                && rule.ports.contains(&origin.port)
                && rule.schemes.contains(&origin.scheme)
        });
        if public || host {
            Ok(())
        } else {
            Err(ErrorCode::Denied)
        }
    }
    pub fn check_address(&self, url: &Url, address: SocketAddr) -> Result<(), ErrorCode> {
        self.check_url(url)?;
        let origin = Origin::of(url)?;
        let http_origin = origin.http_equivalent();
        if address.port() != origin.port {
            return Err(ErrorCode::Denied);
        }
        let ip = address.ip().to_canonical();
        if let Some(grant) = &self.origin
            && grant.origin == http_origin
        {
            if grant.ips.contains(&ip) {
                return Ok(());
            }
            return Err(ErrorCode::Denied);
        }
        // Redirects off a confirmed origin may only reach separately allowed
        // public addresses; they never expand a private IP/origin set.
        if address_kind(ip) == AddressKind::Public {
            Ok(())
        } else {
            Err(ErrorCode::Denied)
        }
    }
    pub fn check_redirect(&self, previous: &Url, next: &Url) -> Result<(), ErrorCode> {
        if matches!(previous.scheme(), "https" | "wss") && !matches!(next.scheme(), "https" | "wss")
        {
            return Err(ErrorCode::Denied);
        }
        self.check_url(next)
    }
}

impl iris_base::net::http_client::ConnectionPolicy for RequestGrant {
    fn check_url(&self, url: &Url) -> anyhow::Result<()> {
        RequestGrant::check_url(self, url).map_err(anyhow::Error::new)
    }
    fn check_address(&self, url: &Url, addr: SocketAddr) -> anyhow::Result<()> {
        RequestGrant::check_address(self, url, addr).map_err(anyhow::Error::new)
    }
    fn cancel(&self) -> &HttpCancel {
        RequestGrant::cancel(self)
    }
    fn reserve_dns(&self) -> anyhow::Result<Box<dyn std::any::Any + Send + Sync>> {
        let lease = RequestGrant::reserve_dns(self).map_err(anyhow::Error::new)?;
        Ok(Box::new(lease))
    }
}

pub(super) fn validate_pins(
    raw: &str,
    kind: TransportKind,
    ips: &[IpAddr],
) -> Result<(Origin, BTreeSet<IpAddr>), ErrorCode> {
    let url = normalize_url(raw, kind)?;
    if ips.is_empty() || ips.len() > 32 {
        return Err(ErrorCode::InvalidArgument);
    }
    let ips: BTreeSet<_> = ips.iter().map(|ip| ip.to_canonical()).collect();
    if ips
        .iter()
        .any(|ip| address_kind(*ip) == AddressKind::Forbidden)
    {
        return Err(ErrorCode::Denied);
    }
    let literal = match url.host() {
        Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip).to_canonical()),
        _ => None,
    };
    if literal.is_some_and(|ip| ips != BTreeSet::from([ip])) {
        return Err(ErrorCode::Denied);
    }
    let private = ips
        .iter()
        .any(|ip| address_kind(*ip) != AddressKind::Public);
    if private
        && ips
            .iter()
            .any(|ip| address_kind(*ip) == AddressKind::Public)
    {
        return Err(ErrorCode::Denied);
    }
    Ok((Origin::of(&url)?, ips))
}
