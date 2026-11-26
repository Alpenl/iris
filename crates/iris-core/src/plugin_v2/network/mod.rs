//! Request-scoped authority. Public requests cannot borrow a private-origin grant.
pub mod consent;
pub(crate) mod cookies;
pub(crate) mod duplex;
mod executor;
mod grant;
pub(crate) mod raw;
mod socket;
pub use crate::vfs::http_client::{AddressKind, address_kind};
pub use consent::parse_consent_origin_url;
pub use executor::NetworkExecutor;
pub use grant::{NetworkGrant, Origin, RequestGrant, TransportKind, normalize_url};

#[cfg(test)]
mod tests {
    mod http;
    mod policy;
    mod socket;
}
