//! Request-scoped authority. Public requests cannot borrow a private-origin grant.
mod address;
pub mod consent;
pub(crate) mod cookies;
mod executor;
mod grant;
mod socket;
pub use address::{AddressKind, address_kind};
pub use executor::NetworkExecutor;
pub use grant::{NetworkGrant, Origin, RequestGrant, TransportKind, normalize_url};

#[cfg(test)]
mod tests {
    mod http;
    mod policy;
    mod socket;
}
