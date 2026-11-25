//! Portable host adapter for deterministic guest state-machine tests. The Wasm
//! adapter imports only Iris capabilities; it has no filesystem or socket API.
use iris_api::v2::{ErrorCode, Id, task::Task};
pub trait Host {
    fn submit(&mut self, task: &Task) -> Result<Id, ErrorCode>;
    fn blob(&mut self, bytes: &[u8]) -> Result<Id, ErrorCode>;
    /// Consume an owned completion blob, copying its bytes and closing it.
    fn take_blob(&mut self, id: Id) -> Result<Vec<u8>, ErrorCode>;
    fn close(&mut self, id: Id);
    fn random(&mut self, _bytes: &mut [u8]) -> Result<(), ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
}
#[cfg(target_arch = "wasm32")]
pub struct WasmHost;
#[cfg(target_arch = "wasm32")]
impl Host for WasmHost {
    fn submit(&mut self, task: &Task) -> Result<Id, ErrorCode> {
        crate::guest::submit(task)
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<Id, ErrorCode> {
        Ok(crate::guest::Blob::new(bytes)?.transfer())
    }
    fn take_blob(&mut self, id: Id) -> Result<Vec<u8>, ErrorCode> {
        crate::guest::Blob::from_completion(id)?.read()
    }
    fn close(&mut self, id: Id) {
        let _ = crate::guest::close(id);
    }
    fn random(&mut self, bytes: &mut [u8]) -> Result<(), ErrorCode> {
        crate::guest::random(bytes)
    }
}
/// Decode only the expected task result shape. A malformed completion cannot be
/// reinterpreted as a different successful operation.
pub fn completed<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, ErrorCode> {
    if value.as_object().is_none_or(|v| v.len() != 1) {
        return Err(ErrorCode::InvalidArgument);
    }
    if let Some(error) = value.get("error") {
        let error: iris_api::v2::task_result::TaskError =
            serde_json::from_value(error.clone()).map_err(|_| ErrorCode::InvalidArgument)?;
        return Err(ErrorCode::from_i32(error.code).unwrap_or(ErrorCode::Internal));
    }
    let value = value.get("ok").ok_or(ErrorCode::InvalidArgument)?;
    serde_json::from_value(value.clone()).map_err(|_| ErrorCode::InvalidArgument)
}
