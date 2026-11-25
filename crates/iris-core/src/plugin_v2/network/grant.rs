use super::{AddressKind, address_kind};
use crate::plugin_v2::{
    instance::next_id,
    ledger::{Activity, ActivityLease, Ledger, Reservation, Resource},
};
use crate::vfs::HttpCancel;
use iris_api::v2::{Capability, ErrorCode, HostRule, Id, NetworkDeclaration};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    Http,
    WebSocket,
    Tcp { tls: bool },
}
impl TransportKind {
    fn capability(self) -> Capability {
        match self {
            Self::Http => Capability::NetHttp,
            Self::WebSocket => Capability::NetWs,
            Self::Tcp { .. } => Capability::NetTcp,
        }
    }
    fn scheme(self, value: &str) -> bool {
        match self {
            Self::Http => matches!(value, "http" | "https"),
            Self::WebSocket => matches!(value, "ws" | "wss"),
            Self::Tcp { tls } => value == if tls { "https" } else { "http" },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}
impl Origin {
    pub fn of(url: &Url) -> Result<Self, ErrorCode> {
        Ok(Self {
            scheme: url.scheme().into(),
            host: normalize_host(url.host_str().ok_or(ErrorCode::InvalidArgument)?)?,
            port: url
                .port_or_known_default()
                .ok_or(ErrorCode::InvalidArgument)?,
        })
    }
    pub fn display(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{}:{}", self.scheme, host, self.port)
    }
    pub fn http_equivalent(&self) -> Self {
        let scheme = match self.scheme.as_str() {
            "ws" => "http",
            "wss" => "https",
            other => other,
        };
        Self {
            scheme: scheme.into(),
            host: self.host.clone(),
            port: self.port,
        }
    }
}

struct ConfirmedOrigin {
    origin: Origin,
    ips: BTreeSet<IpAddr>,
    private: bool,
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

    /// The UI supplies the exact displayed Origin and resolved address set after
    /// confirmation. No discovery/redirect/plugin-form code may call this.
    pub fn confirm_origin(
        &self,
        raw: &str,
        kind: TransportKind,
        ips: &[IpAddr],
    ) -> Result<Id, ErrorCode> {
        self.check_live()?;
        let allowed = self.0.capabilities.contains(&kind.capability())
            || (kind == TransportKind::Http
                && self
                    .0
                    .capabilities
                    .iter()
                    .any(|c| matches!(c, Capability::NetHttp | Capability::NetWs)));
        if !allowed {
            return Err(ErrorCode::Denied);
        }
        let (origin, ips, private) = validate_pins(raw, kind, ips)?;
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
                private,
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
        self.request_tracked(raw, kind, origin, Activity::default())
    }
    pub(crate) fn request_tracked(
        &self,
        raw: &str,
        kind: TransportKind,
        origin: Option<Id>,
        activity: Activity,
    ) -> Result<Arc<RequestGrant>, ErrorCode> {
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
        Ok(request)
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
        let allowed = self.0.capabilities.contains(&kind.capability())
            || (kind == TransportKind::Http
                && self
                    .0
                    .capabilities
                    .iter()
                    .any(|c| matches!(c, Capability::NetHttp | Capability::NetWs)));
        if !allowed {
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
        if let Some(grant) = &self.origin {
            if grant.origin == http_origin {
                if grant.ips.contains(&ip) {
                    return Ok(());
                }
                return Err(ErrorCode::Denied);
            }
            // A private-service redirect may use separately allowed public CDNs;
            // it never expands its private IP/origin set.
            if grant.private && address_kind(ip) != AddressKind::Public {
                return Err(ErrorCode::Denied);
            }
        }
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

pub fn normalize_url(raw: &str, kind: TransportKind) -> Result<Url, ErrorCode> {
    if raw.len() > 8192
        || raw.trim() != raw
        || raw.bytes().any(|c| c <= 0x20 || c == 0x7f || c == b'\\')
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut url = Url::parse(raw).map_err(|_| ErrorCode::InvalidArgument)?;
    if !kind.scheme(url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
        || url.port_or_known_default() == Some(0)
    {
        return Err(ErrorCode::Denied);
    }
    let host = normalize_host(url.host_str().ok_or(ErrorCode::InvalidArgument)?)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    url.set_host(Some(&host))
        .map_err(|_| ErrorCode::InvalidArgument)?;
    url.set_fragment(None);
    Ok(url)
}

pub(super) fn validate_pins(
    raw: &str,
    kind: TransportKind,
    ips: &[IpAddr],
) -> Result<(Origin, BTreeSet<IpAddr>, bool), ErrorCode> {
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
    Ok((Origin::of(&url)?, ips, private))
}

fn normalize_host(raw: &str) -> Result<String, ErrorCode> {
    let raw = raw.trim_end_matches('.');
    if raw.is_empty() || raw.len() > 253 {
        return Err(ErrorCode::InvalidArgument);
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    let host = Host::parse(raw).map_err(|_| ErrorCode::InvalidArgument)?;
    Ok(match host {
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
        Host::Domain(host) => {
            if host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }) {
                return Err(ErrorCode::InvalidArgument);
            }
            host.to_ascii_lowercase()
        }
    })
}
fn matches_host(rule: &str, host: &str) -> bool {
    if let Some(base) = rule.strip_prefix("*.") {
        host.strip_suffix(base)
            .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
    } else {
        rule == host
    }
}
fn normalize_rule(rule: &mut HostRule, tcp: bool) -> Result<(), ErrorCode> {
    let wildcard = rule.host.starts_with("*.");
    let base = normalize_host(rule.host.strip_prefix("*.").unwrap_or(&rule.host))?;
    if wildcard && (tcp || base.parse::<IpAddr>().is_ok()) {
        return Err(ErrorCode::InvalidArgument);
    }
    rule.host = if wildcard { format!("*.{base}") } else { base };
    if rule.ports.is_empty()
        || rule.ports.len() > 32
        || rule.ports.contains(&0)
        || rule.schemes.is_empty()
        || rule.schemes.len() > 4
        || rule.schemes.iter().any(|s| {
            if tcp {
                !matches!(s.as_str(), "tcp" | "tls")
            } else {
                !matches!(s.as_str(), "http" | "https" | "ws" | "wss")
            }
        })
    {
        return Err(ErrorCode::InvalidArgument);
    }
    rule.ports.sort_unstable();
    rule.ports.dedup();
    rule.schemes.sort();
    rule.schemes.dedup();
    Ok(())
}
pub(crate) fn validate_declaration(value: &mut NetworkDeclaration) -> Result<(), ErrorCode> {
    if value.net_hosts.len() > 128
        || value.tcp_targets.len() > 32
        || value.public_ports.len() > 32
        || value.public_ports.contains(&0)
    {
        return Err(ErrorCode::QuotaExceeded);
    }
    for rule in &mut value.net_hosts {
        normalize_rule(rule, false)?;
    }
    for rule in &mut value.tcp_targets {
        normalize_rule(rule, true)?;
    }
    value.public_ports.sort_unstable();
    value.public_ports.dedup();
    Ok(())
}
