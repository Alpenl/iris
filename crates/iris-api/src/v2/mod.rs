//! Versioned Wasm contracts. No host I/O or JSON implementation dependencies.

pub mod crypto;
pub mod enhance;
mod integer;
pub mod manifest;
pub mod source;
pub mod stream;
pub mod task;
pub mod task_result;
pub mod wire;
pub use wire::{HostCommand, HostEvent, LogLevel};

pub use integer::Id;
pub use manifest::{Capability, HostRule, Manifest, NetworkDeclaration};
use serde::{Deserialize, Serialize};

pub const ABI_VERSION: u32 = 2;
pub const MESSAGE_LIMIT: usize = 64 * 1024;
pub const CHUNK_LIMIT: usize = 1024 * 1024;
pub const JSON_DEPTH_LIMIT: usize = 32;
pub const COMMAND_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    InvalidArgument = -1,
    Denied = -2,
    QuotaExceeded = -3,
    Backpressure = -4,
    Cancelled = -5,
    Timeout = -6,
    StaleHandle = -7,
    Unsupported = -8,
    IoError = -9,
    AuthFailed = -10,
    RangeInvalid = -11,
    BadState = -12,
    Internal = -13,
    OutOfWindow = -14,
}

impl ErrorCode {
    pub const fn from_i32(code: i32) -> Option<Self> {
        Some(match code {
            -1 => Self::InvalidArgument,
            -2 => Self::Denied,
            -3 => Self::QuotaExceeded,
            -4 => Self::Backpressure,
            -5 => Self::Cancelled,
            -6 => Self::Timeout,
            -7 => Self::StaleHandle,
            -8 => Self::Unsupported,
            -9 => Self::IoError,
            -10 => Self::AuthFailed,
            -11 => Self::RangeInvalid,
            -12 => Self::BadState,
            -13 => Self::Internal,
            -14 => Self::OutOfWindow,
            _ => return None,
        })
    }

    pub const fn as_i64(self) -> i64 {
        self as i32 as i64
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ErrorCode {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Control,
    Stream,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestError {
    pub code: i32,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Reply<C> {
    Ok { ok: Vec<C> },
    Error { error: GuestError },
}

pub const fn pack(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | len as u64
}
pub const fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub media: Id,
    pub seek: Id,
    pub context: Id,
}
