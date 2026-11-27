//! Bounded URL contribution matching and pure URL/Origin/header policy checks.
//! Patterns select a resolver; they never grant network access. Queries/fragments
//! are not part of a routing pattern.
mod policy;
pub use policy::{Origin, TransportKind, normalize_url, parse_consent_origin_url};
pub(crate) use policy::{matches_host, validate_declaration, validate_headers};

pub fn valid_pattern(pattern: &str) -> bool {
    parse_pattern(pattern, "https").is_some()
}

struct ParsedPattern<'a> {
    scheme: &'a str,
    suffix: bool,
    wildcard: bool,
    rule_host: String,
    rule_port: Option<u16>,
    path: String,
    is_prefix: bool,
}

fn parse_pattern<'a>(pattern: &'a str, fallback_scheme: &str) -> Option<ParsedPattern<'a>> {
    if pattern.len() > 2048 {
        return None;
    }
    let (scheme, rest) = pattern.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "*") {
        return None;
    }
    let (authority, raw_path) = rest.split_once('/')?;
    let (suffix, authority) = authority
        .strip_prefix("*.")
        .map_or((false, authority), |a| (true, a));
    let wildcard = authority == "*";
    if (suffix && wildcard) || raw_path.contains(['?', '#']) {
        return None;
    }
    let pattern_path = format!("/{raw_path}");
    let is_prefix = pattern_path.ends_with('*');
    let bare_path = pattern_path.strip_suffix('*').unwrap_or(&pattern_path);
    if bare_path.contains('*')
        || authority.contains(['@', '\\', '?', '#'])
        || authority.chars().any(char::is_control)
    {
        return None;
    }
    let address = if wildcard {
        "pattern.invalid"
    } else {
        authority
    };
    let effective_scheme = if scheme == "*" {
        fallback_scheme
    } else {
        scheme
    };
    let rule = url::Url::parse(&format!("{effective_scheme}://{address}{bare_path}")).ok()?;
    let rule_host = rule.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if suffix && (!rule_host.contains('.') || rule_host.parse::<std::net::IpAddr>().is_ok()) {
        return None;
    }
    Some(ParsedPattern {
        scheme,
        suffix,
        wildcard,
        rule_host,
        rule_port: rule.port_or_known_default(),
        path: rule.path().to_owned(),
        is_prefix,
    })
}

pub fn matches(pattern: &str, raw: &str) -> bool {
    if raw.len() > 8192 {
        return false;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Some(ParsedPattern {
        scheme,
        suffix,
        wildcard,
        rule_host,
        rule_port,
        path,
        is_prefix,
    }) = parse_pattern(pattern, url.scheme())
    else {
        return false;
    };
    if scheme != "*" && scheme != url.scheme() {
        return false;
    }
    if rule_port != url.port_or_known_default() {
        return false;
    }
    let Some(host) = url.host_str().map(|h| h.trim_end_matches('.')) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host_ok = wildcard
        || if suffix {
            host.strip_suffix(&rule_host)
                .is_some_and(|left| left.len() > 1 && left.ends_with('.'))
        } else {
            host == rule_host
        };
    host_ok
        && if is_prefix {
            url.path().starts_with(&path)
        } else {
            url.path() == path
        }
}

#[cfg(test)]
mod tests {
    use super::matches;
    #[test]
    fn url_routes_require_host_boundaries_and_explicit_nondefault_ports() {
        assert!(matches(
            "https://*.example.com/watch*",
            "https://video.example.com/watch/7?token=private"
        ));
        for url in [
            "https://example.com/watch/7",
            "https://notexample.com/watch/7",
            "https://video.example.com.evil/watch/7",
            "http://video.example.com/watch/7",
            "https://video.example.com:8443/watch/7",
            "https://user:pass@video.example.com/watch/7",
        ] {
            assert!(!matches("https://*.example.com/watch*", url), "{url}");
        }
        assert!(matches(
            "*://example.com:8443/*",
            "http://example.com:8443/a"
        ));
        assert!(matches("https://*/*", "https://another.example/a"));
        assert!(!matches(
            "https://example.com/*?token=*",
            "https://example.com/a?token=x"
        ));
        assert!(!matches(
            "https://example.com/a*b",
            "https://example.com/axb"
        ));
    }
}
