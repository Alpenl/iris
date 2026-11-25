//! Safe high-level guest helpers and shared strict wire validation.
pub use iris_api as api;
pub use serde_json;
pub mod client;
pub mod crypto;
pub mod json;

#[cfg(target_arch = "wasm32")]
pub mod guest;

/// Export a single-threaded callback. The function receives owned JSON and must
/// return a protocol reply; asynchronous work uses the host submission API.
#[macro_export]
macro_rules! export_plugin {
    ($handler:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_abi_version() -> i32 {
            2
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn iris_alloc(len: u32) -> u32 {
            $crate::guest::allocate(len)
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_dealloc(ptr: u32, len: u32) {
            // SAFETY: the host returns exactly the SDK allocation's ptr/len.
            unsafe { $crate::guest::deallocate(ptr, len) }
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn iris_on_event(ptr: u32, len: u32) -> u64 {
            // SAFETY: the host owns a live allocation throughout this callback.
            unsafe { $crate::guest::dispatch(ptr, len, $handler) }
        }
    };
}
