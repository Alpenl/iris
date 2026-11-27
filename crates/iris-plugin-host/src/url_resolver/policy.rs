//! Pure URL, Origin, network declaration, and HTTP header policy validation
//! shared by manifest/command validation and runtime network enforcement.
use iris_api::v2::{Capability, ErrorCode, HostRule, NetworkDeclaration};
use iris_base::net::http_client::{HeaderValidationError, validate_header_pairs};
use std::net::IpAddr;
use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportKind {
    Http,
    WebSocket,
    Tcp { tls: bool },
}

impl TransportKind {
    pub(crate) fn capability(self) -> Capability {
        match self {
            Self::Http => Capability::NetHttp,
            Self::WebSocket => Capability::NetWs,
            Self::Tcp { .. } => Capability::NetTcp,
        }
    }

    pub(crate) fn scheme(self, value: &str) -> bool {
        match self {
            Self::Http => matches!(value, "http" | "https"),
            Self::WebSocket => matches!(value, "ws" | "wss"),
            Self::Tcp { tls } => value == if tls { "https" } else { "http" },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl Origin {
    pub(crate) fn of(url: &Url) -> Result<Self, ErrorCode> {
        Ok(Self {
            scheme: url.scheme().into(),
            host: normalize_host(url.host_str().ok_or(ErrorCode::InvalidArgument)?)?,
            port: url
                .port_or_known_default()
                .ok_or(ErrorCode::InvalidArgument)?,
        })
    }

    pub(crate) fn display(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{}:{}", self.scheme, host, self.port)
    }

    pub(crate) fn http_equivalent(&self) -> Self {
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

pub(crate) fn normalize_url(raw: &str, kind: TransportKind) -> Result<Url, ErrorCode> {
    if raw.len() > 8192
        || raw.trim() != raw
        || raw.bytes().any(|c| c <= 0x20 || c == 0x7f || c == b'\\')
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let rewritten;
    let effective = match kind {
        TransportKind::Tcp { tls: false } if raw.starts_with("tcp://") => {
            if Url::parse(raw)
                .map_err(|_| ErrorCode::InvalidArgument)?
                .port()
                .is_none()
            {
                return Err(ErrorCode::InvalidArgument);
            }
            rewritten = format!("http://{}", &raw["tcp://".len()..]);
            rewritten.as_str()
        }
        TransportKind::Tcp { tls: true } if raw.starts_with("tls://") => {
            if Url::parse(raw)
                .map_err(|_| ErrorCode::InvalidArgument)?
                .port()
                .is_none()
            {
                return Err(ErrorCode::InvalidArgument);
            }
            rewritten = format!("https://{}", &raw["tls://".len()..]);
            rewritten.as_str()
        }
        _ => raw,
    };
    let mut url = Url::parse(effective).map_err(|_| ErrorCode::InvalidArgument)?;
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

pub(crate) fn parse_consent_origin_url(raw: &str) -> Result<(Origin, TransportKind), ErrorCode> {
    for kind in [
        TransportKind::Http,
        TransportKind::WebSocket,
        TransportKind::Tcp { tls: false },
        TransportKind::Tcp { tls: true },
    ] {
        if let Ok(url) = normalize_url(raw, kind) {
            return Ok((Origin::of(&url)?.http_equivalent(), kind));
        }
    }
    Err(ErrorCode::Denied)
}

pub(crate) fn normalize_host(raw: &str) -> Result<String, ErrorCode> {
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

pub(crate) fn matches_host(rule: &str, host: &str) -> bool {
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

pub(crate) fn validate_headers(headers: &[(String, String)]) -> Result<(), ErrorCode> {
    validate_header_pairs(headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))).map_err(
        |(_name, kind)| match kind {
            HeaderValidationError::QuotaExceeded => ErrorCode::QuotaExceeded,
            HeaderValidationError::InvalidName | HeaderValidationError::InvalidValue => {
                ErrorCode::InvalidArgument
            }
            HeaderValidationError::ForbiddenHeader => ErrorCode::Denied,
        },
    )
}
