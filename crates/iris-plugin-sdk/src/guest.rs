//! These are guest-local pointers. The Wasm sandbox, not Rust references, is the
//! host isolation boundary. No guest memory borrow survives a host callback.
use iris_api::v2::{
    CHUNK_LIMIT, ErrorCode, Id, MESSAGE_LIMIT, Reply, crypto::CryptoSpec, task::Task,
};
use serde::Serialize;

#[link(wasm_import_module = "iris_v2")]
unsafe extern "C" {
    fn host_submit(ptr: u32, len: u32) -> i64;
    fn host_blob_new(ptr: u32, len: u32) -> i64;
    fn host_blob_len(handle: i64) -> i64;
    fn host_blob_read(handle: i64, offset: i64, dst: u32, cap: u32) -> i64;
    fn host_close(handle: i64) -> i32;
    fn host_stream_reply(request: i64, blob: i64, flags: i32) -> i64;
    fn host_stream_fail(request: i64, code: i32) -> i32;
    fn host_crypto(
        spec_ptr: u32,
        spec_len: u32,
        in_ptr: u32,
        in_len: u32,
        out_ptr: u32,
        out_cap: u32,
    ) -> i64;
    fn host_random(dst: u32, len: u32) -> i32;
    fn host_clock(kind: i32) -> i64;
}

fn result(value: i64) -> Result<u64, ErrorCode> {
    if value < 0 {
        Err(i32::try_from(value)
            .ok()
            .and_then(ErrorCode::from_i32)
            .unwrap_or(ErrorCode::Internal))
    } else {
        Ok(value as u64)
    }
}

pub fn submit(task: &Task) -> Result<Id, ErrorCode> {
    let bytes = crate::json::encode(task, MESSAGE_LIMIT)?;
    // SAFETY: byte storage remains live until the synchronous import returns.
    let id = result(unsafe { host_submit(bytes.as_ptr() as u32, bytes.len() as u32) })?;
    if id == 0 {
        Err(ErrorCode::Internal)
    } else {
        Ok(Id(id))
    }
}

/// Close an operation or resource in the active host window scope.
pub fn close(id: Id) -> Result<(), ErrorCode> {
    let id = i64::try_from(id.0).map_err(|_| ErrorCode::InvalidArgument)?;
    if id == 0 {
        return Err(ErrorCode::InvalidArgument);
    }
    // SAFETY: integer capability ownership and generation are checked by the host.
    result(unsafe { host_close(id) } as i64).map(|_| ())
}

pub struct Blob(i64);
impl Blob {
    pub fn new(bytes: &[u8]) -> Result<Self, ErrorCode> {
        if bytes.len() > CHUNK_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        // SAFETY: the host copies these bytes synchronously.
        let handle = result(unsafe { host_blob_new(bytes.as_ptr() as u32, bytes.len() as u32) })?;
        if handle == 0 {
            Err(ErrorCode::Internal)
        } else {
            Ok(Self(handle as i64))
        }
    }
    /// Adopt an owned blob handle from a completion event exactly once.
    pub fn from_completion(id: Id) -> Result<Self, ErrorCode> {
        if id.0 == 0 || id.0 > i64::MAX as u64 {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Self(id.0 as i64))
    }
    pub fn id(&self) -> Id {
        Id(self.0 as u64)
    }
    /// Move this handle into a returned command (manifest, context or cue Blob).
    /// It is consumed by the host when that command batch is accepted.
    pub fn transfer(mut self) -> Id {
        let id = self.id();
        self.0 = 0;
        id
    }
    pub fn read(&self) -> Result<Vec<u8>, ErrorCode> {
        // SAFETY: integer-only imports validate handle ownership in the host.
        let len = result(unsafe { host_blob_len(self.0) })? as usize;
        if len > CHUNK_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut bytes = vec![0; len];
        let mut offset = 0;
        while offset < len {
            // SAFETY: the destination is a valid writable allocation.
            let n = result(unsafe {
                host_blob_read(
                    self.0,
                    offset as i64,
                    bytes[offset..].as_mut_ptr() as u32,
                    (len - offset) as u32,
                )
            })? as usize;
            if n == 0 || n > len - offset {
                return Err(ErrorCode::Internal);
            }
            offset += n;
        }
        Ok(bytes)
    }
    /// On success the host consumes the blob; on failure it remains available.
    pub fn reply(&mut self, request: Id, eof: bool) -> Result<usize, ErrorCode> {
        let request = i64::try_from(request.0).map_err(|_| ErrorCode::InvalidArgument)?;
        // SAFETY: opaque integer IDs are checked by the host.
        let count = result(unsafe { host_stream_reply(request, self.0, i32::from(eof)) })?;
        self.0 = 0;
        Ok(count as usize)
    }
}
impl Drop for Blob {
    fn drop(&mut self) {
        if self.0 > 0 {
            // SAFETY: close validates ownership; stale handles have no effect.
            unsafe {
                host_close(self.0);
            }
        }
    }
}

pub fn fail_stream(request: Id, error: ErrorCode) -> Result<(), ErrorCode> {
    let id = i64::try_from(request.0).map_err(|_| ErrorCode::InvalidArgument)?;
    // SAFETY: integer-only import.
    result(unsafe { host_stream_fail(id, error as i32) } as i64).map(|_| ())
}

pub fn crypto(spec: &CryptoSpec, input: &[u8], capacity: usize) -> Result<Vec<u8>, ErrorCode> {
    if input.len() > CHUNK_LIMIT || capacity > CHUNK_LIMIT + 16 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let spec = serde_json::to_vec(spec).map_err(|_| ErrorCode::InvalidArgument)?;
    let mut output = vec![0; capacity];
    // SAFETY: all ranges are valid and input/output storage does not overlap.
    let n = result(unsafe {
        host_crypto(
            spec.as_ptr() as u32,
            spec.len() as u32,
            input.as_ptr() as u32,
            input.len() as u32,
            output.as_mut_ptr() as u32,
            capacity as u32,
        )
    })? as usize;
    if n > capacity {
        return Err(ErrorCode::Internal);
    }
    output.truncate(n);
    Ok(output)
}

pub fn random(bytes: &mut [u8]) -> Result<(), ErrorCode> {
    if bytes.len() > MESSAGE_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    // SAFETY: destination is writable for the requested size.
    result(unsafe { host_random(bytes.as_mut_ptr() as u32, bytes.len() as u32) } as i64).map(|_| ())
}
pub fn clock(monotonic: bool) -> Result<Id, ErrorCode> {
    // SAFETY: no pointers or resource handles are supplied.
    result(unsafe { host_clock(i32::from(monotonic)) }).map(Id)
}

#[doc(hidden)]
pub fn allocate(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    Box::into_raw(vec![0u8; len as usize].into_boxed_slice()) as *mut u8 as u32
}

/// # Safety
/// `ptr,len` must be a live SDK allocation, returned exactly once by the host.
#[doc(hidden)]
pub unsafe fn deallocate(ptr: u32, len: u32) {
    if ptr == 0 && len == 0 {
        return;
    }
    let raw = std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len as usize);
    // SAFETY: the ABI caller guarantees the allocation identity and layout.
    unsafe {
        drop(Box::from_raw(raw));
    }
}

/// # Safety
/// The caller supplies a live input allocation borrowed for this call only.
#[doc(hidden)]
pub unsafe fn dispatch<C: Serialize>(
    ptr: u32,
    len: u32,
    handler: fn(serde_json::Value) -> Reply<C>,
) -> u64 {
    if len as usize > MESSAGE_LIMIT {
        return 0;
    }
    // SAFETY: host allocated input and retains ownership until return.
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let response = match crate::json::decode(input, MESSAGE_LIMIT) {
        Ok(event) => handler(event),
        Err(error) => Reply::Error {
            error: iris_api::v2::GuestError {
                code: error as i32,
                message: "invalid event".into(),
            },
        },
    };
    let output = match crate::json::encode(&response, MESSAGE_LIMIT) {
        Ok(output) => output,
        Err(error) => {
            let fallback: Reply<()> = Reply::Error {
                error: iris_api::v2::GuestError {
                    code: error as i32,
                    message: "encoded reply exceeded message limit".into(),
                },
            };
            let Ok(output) = crate::json::encode(&fallback, MESSAGE_LIMIT) else {
                return 0;
            };
            output
        }
    };
    if output.len() > MESSAGE_LIMIT {
        return 0;
    }
    let len = output.len() as u32;
    let ptr = Box::into_raw(output.into_boxed_slice()) as *mut u8 as u32;
    iris_api::v2::pack(ptr, len)
}
