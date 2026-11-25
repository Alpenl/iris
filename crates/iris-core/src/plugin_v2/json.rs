//! Protocol-2 JSON codec adapter mapping [`crate::bounded_json`] errors to [`ErrorCode`].

use crate::bounded_json::{self, BoundedJsonError};
use iris_api::v2::ErrorCode;
use serde::{Serialize, de::DeserializeOwned};

const fn map_err(err: BoundedJsonError) -> ErrorCode {
    match err {
        BoundedJsonError::QuotaExceeded => ErrorCode::QuotaExceeded,
        BoundedJsonError::Invalid => ErrorCode::InvalidArgument,
    }
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, ErrorCode> {
    bounded_json::decode(bytes, limit).map_err(map_err)
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, ErrorCode> {
    bounded_json::encode(value, limit).map_err(map_err)
}

pub fn decode_exact<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
    limit: usize,
) -> Result<T, ErrorCode> {
    bounded_json::decode_exact(bytes, limit).map_err(map_err)
}
