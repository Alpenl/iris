//! Safe high-level guest helpers and shared strict wire validation.
//!
//! Note: [`iris_api::v2::crypto::CryptoSpec`] and the `iris_v2.host_crypto` Wasm
//! import remain part of the ABI v2 specification for low-level / non-Rust guests,
//! while this SDK focuses on typed event dispatch, tasks, blobs, and bounded JSON.
pub use iris_api as api;
pub use serde_json;
pub mod client;
pub mod json;

#[cfg(target_arch = "wasm32")]
pub mod guest;

/// Static wire error labels avoiding `core::fmt::Debug` table bloat in Wasm guests.
pub const fn error_message(code: iris_api::v2::ErrorCode) -> &'static str {
    use iris_api::v2::ErrorCode;
    match code {
        ErrorCode::InvalidArgument => "InvalidArgument",
        ErrorCode::Denied => "Denied",
        ErrorCode::QuotaExceeded => "QuotaExceeded",
        ErrorCode::Backpressure => "Backpressure",
        ErrorCode::Cancelled => "Cancelled",
        ErrorCode::Timeout => "Timeout",
        ErrorCode::StaleHandle => "StaleHandle",
        ErrorCode::Unsupported => "Unsupported",
        ErrorCode::IoError => "IoError",
        ErrorCode::AuthFailed => "AuthFailed",
        ErrorCode::RangeInvalid => "RangeInvalid",
        ErrorCode::BadState => "BadState",
        ErrorCode::Internal => "Internal",
        ErrorCode::OutOfWindow => "OutOfWindow",
    }
}

/// Export a single-threaded callback receiving raw JSON (`serde_json::Value`).
/// Prefer [`export_typed_plugin!`] for standard ABI v2 plugins.
#[macro_export]
macro_rules! export_plugin {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_abi_version() -> i32 {
            2
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_alloc(len: u32) -> u32 {
            $crate::guest::allocate(len)
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_dealloc(ptr: u32, len: u32) {
            // SAFETY: the host returns exactly the SDK allocation's ptr/len.
            unsafe { $crate::guest::deallocate(ptr, len) }
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_on_event(ptr: u32, len: u32) -> u64 {
            // SAFETY: the host owns a live allocation throughout this callback.
            unsafe { $crate::guest::dispatch(ptr, len, $handler) }
        }
        #[cfg(not(target_arch = "wasm32"))]
        const _: fn() = || {
            fn check<C>(_: fn($crate::serde_json::Value) -> $crate::api::v2::Reply<C>) {}
            check($handler);
        };
    };
}

/// Export a typed ABI v2 plugin without intermediate `serde_json::Value` parsing
/// or `format!("{code:?}")` binary bloat.
///
/// Forms:
/// - `export_typed_plugin!(handler_fn);` — stateless `fn(E) -> Result<Vec<C>, ErrorCode>`.
/// - `export_typed_plugin!(PluginType, |plugin, event| expr);` — Wasm-gated stateful
///   `thread_local!` export returning `Result<Vec<HostCommand>, ErrorCode>`.
#[macro_export]
macro_rules! export_typed_plugin {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_abi_version() -> i32 {
            2
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_alloc(len: u32) -> u32 {
            $crate::guest::allocate(len)
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_dealloc(ptr: u32, len: u32) {
            // SAFETY: the host returns exactly the SDK allocation's ptr/len.
            unsafe { $crate::guest::deallocate(ptr, len) }
        }
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_on_event(ptr: u32, len: u32) -> u64 {
            // SAFETY: the host owns a live allocation throughout this callback.
            unsafe { $crate::guest::dispatch_typed(ptr, len, $handler) }
        }
        #[cfg(not(target_arch = "wasm32"))]
        const _: fn() = || {
            fn check<E, C>(_: fn(E) -> Result<Vec<C>, $crate::api::v2::ErrorCode>) {}
            check($handler);
        };
    };
    ($plugin:ty, |$state:ident, $event:ident| $body:expr) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            std::thread_local! {
                static PLUGIN: std::cell::RefCell<$plugin> =
                    std::cell::RefCell::new(<$plugin>::default());
            }
            fn dispatch(
                $event: $crate::api::v2::HostEvent<$crate::serde_json::Value>,
            ) -> Result<Vec<$crate::api::v2::HostCommand>, $crate::api::v2::ErrorCode> {
                PLUGIN.with_borrow_mut(|$state| $body)
            }
            $crate::export_typed_plugin!(dispatch);
        };
    };
}

#[cfg(test)]
mod tests {
    use super::error_message;
    use crate::json::decode;
    use iris_api::v2::{ErrorCode, HostCommand, HostEvent, Id, MESSAGE_LIMIT};
    use serde_json::Value;

    fn stateless_handler(_event: HostEvent<Value>) -> Result<Vec<HostCommand>, ErrorCode> {
        Ok(Vec::new())
    }
    crate::export_typed_plugin!(stateless_handler);

    #[test]
    fn error_messages_match_debug_variant_names() {
        for raw in -14..=-1 {
            let code = ErrorCode::from_i32(raw).expect("valid error code");
            assert_eq!(error_message(code), format!("{code:?}"));
        }
    }

    #[test]
    fn typed_host_event_decodes_directly_without_value_roundtrip() {
        let raw = br#"{"event":"window_closed","window":"42"}"#;
        let event: HostEvent<Value> = decode(raw, MESSAGE_LIMIT).unwrap();
        assert!(matches!(event, HostEvent::WindowClosed { window: Id(42) }));
    }
}
