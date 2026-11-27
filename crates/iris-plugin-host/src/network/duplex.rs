//! Convert an already authorized connection into an owned nonblocking transport.
use super::{RequestGrant, raw};
use iris_api::v2::ErrorCode;
use iris_net::http_client::{HttpCancel, NetworkScope, connect_transport};
pub(crate) use iris_net::wire::Wire;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use url::Url;

pub(crate) fn connect(
    url: &Url,
    grant: Arc<RequestGrant>,
    cancel: &HttpCancel,
) -> Result<Wire, ErrorCode> {
    let scope =
        NetworkScope::authorized(url.clone(), grant.clone()).map_err(|_| ErrorCode::Denied)?;
    let (transport, registration) = connect_transport(
        url,
        &scope,
        cancel,
        Some(Instant::now() + Duration::from_secs(10)),
    )
    .map_err(|e| raw::transport_error(e, &grant, cancel))?;
    Ok(Wire::from_transport(transport, registration))
}
