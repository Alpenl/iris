use super::Origin;
use crate::ledger::{Ledger, Reservation, Resource};
use crate::network::raw::{Cookies, Headers};
use iris_api::v2::ErrorCode;
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Clone)]
struct Cookie {
    value: String,
    expires: Option<SystemTime>,
    secure: bool,
}
type Jar = BTreeMap<(Origin, String, String), Cookie>;
pub(crate) struct CookieJar {
    jar: Mutex<Jar>,
    _memory: Reservation,
}
impl CookieJar {
    pub(crate) fn new(owner: &str, ledger: &Ledger) -> Result<Self, ErrorCode> {
        Ok(Self {
            jar: Mutex::new(BTreeMap::new()),
            _memory: ledger.reserve(owner, &[(Resource::HostBytes, 128 << 10)])?,
        })
    }
}
impl Cookies for CookieJar {
    fn header(&self, url: &Url) -> Option<String> {
        let origin = Origin::of(url).ok()?.http_equivalent();
        let mut jar = self.jar.lock().unwrap_or_else(|e| e.into_inner());
        jar.retain(|_, cookie| cookie.expires.is_none_or(|time| time > SystemTime::now()));
        let mut matches: Vec<_> = jar
            .iter()
            .filter(|((scope, _, path), cookie)| {
                scope == &origin
                    && (!cookie.secure || matches!(url.scheme(), "https" | "wss"))
                    && (url.path() == path
                        || url
                            .path()
                            .strip_prefix(path)
                            .is_some_and(|rest| path.ends_with('/') || rest.starts_with('/')))
            })
            .collect();
        matches.sort_by_key(|((_, name, path), _)| (std::cmp::Reverse(path.len()), name.as_str()));
        if matches.is_empty() {
            return None;
        }
        Some(
            matches
                .into_iter()
                .map(|((_, name, _), cookie)| format!("{name}={}", cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
    fn receive(&self, url: &Url, headers: &Headers) -> Result<(), ErrorCode> {
        let origin = Origin::of(url)?.http_equivalent();
        let mut jar = self.jar.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = jar.clone();
        next.retain(|_, cookie| cookie.expires.is_none_or(|time| time > SystemTime::now()));
        let mut total_bytes: usize = next.iter().map(|(k, c)| cost(k, c)).sum();
        for (_, field) in headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        {
            if field.len() > 4096 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let Some((name, path, cookie)) = parse(url, &origin, field) else {
                continue;
            };
            let key = (origin.clone(), name, path);
            if cookie.expires.is_some_and(|time| time <= SystemTime::now()) {
                if let Some(removed) = next.remove(&key) {
                    total_bytes -= cost(&key, &removed);
                }
            } else {
                let old = next.get(&key).map_or(0, |old| cost(&key, old));
                let added = cost(&key, &cookie);
                let updated = total_bytes - old + added;
                if (!next.contains_key(&key) && next.len() >= 128) || updated > 65536 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                total_bytes = updated;
                next.insert(key, cookie);
            }
        }
        *jar = next;
        Ok(())
    }
}

fn cost((origin, name, path): &(Origin, String, String), cookie: &Cookie) -> usize {
    origin.host.len() + origin.scheme.len() + name.len() + path.len() + cookie.value.len()
}

fn parse(url: &Url, origin: &Origin, field: &str) -> Option<(String, String, Cookie)> {
    let mut parts = field.split(';');
    let (name, value) = parts.next()?.trim().split_once('=')?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        || !value.bytes().all(|b| {
            b == 0x21
                || (0x23..=0x2b).contains(&b)
                || (0x2d..=0x3a).contains(&b)
                || (0x3c..=0x5b).contains(&b)
                || (0x5d..=0x7e).contains(&b)
        })
    {
        return None;
    }
    let mut cookie = Cookie {
        value: value.into(),
        expires: None,
        secure: false,
    };
    let mut path = url
        .path()
        .rsplit_once('/')
        .map(|(p, _)| if p.is_empty() { "/" } else { p })
        .unwrap_or("/")
        .to_owned();
    let mut max_age = None;
    let mut samesite_none = false;
    for attribute in parts {
        let (key, value) = attribute
            .trim()
            .split_once('=')
            .unwrap_or((attribute.trim(), ""));
        match key.to_ascii_lowercase().as_str() {
            "domain"
                if !value
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(&origin.host) =>
            {
                return None;
            }
            "path" if value.starts_with('/') => path = value.to_owned(),
            "secure" => cookie.secure = true,
            "samesite" if value.trim().eq_ignore_ascii_case("none") => samesite_none = true,
            "max-age" => max_age = value.parse::<i64>().ok(),
            "expires" => cookie.expires = httpdate::parse_http_date(value).ok(),
            _ => {}
        }
    }
    if samesite_none && !cookie.secure {
        return None;
    }
    if let Some(age) = max_age {
        cookie.expires = Some(if age <= 0 {
            UNIX_EPOCH
        } else {
            SystemTime::now().checked_add(Duration::from_secs(age as u64))?
        });
    }
    if cookie.secure && !matches!(url.scheme(), "https" | "wss") {
        return None;
    }
    if name.starts_with("__Secure-") && !cookie.secure {
        return None;
    }
    if name.starts_with("__Host-")
        && (!cookie.secure || path != "/" || field.to_ascii_lowercase().contains("domain="))
    {
        return None;
    }
    Some((name.into(), path, cookie))
}
