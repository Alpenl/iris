//! Request-scoped authority. Public requests cannot borrow a private-origin grant.
pub(crate) mod consent;
pub(crate) mod cookies;
pub(crate) mod duplex;
mod executor;
mod grant;
pub(crate) mod raw;
mod socket;
pub(crate) use crate::url_resolver::{
    Origin, TransportKind, normalize_url, parse_consent_origin_url,
};
pub(crate) use executor::NetworkExecutor;
pub(crate) use grant::{NetworkGrant, RequestGrant};
pub(crate) use iris_base::net::http_client::{AddressKind, address_kind};

#[cfg(test)]
mod tests {
    mod http;
    mod policy;
    mod socket;
}
