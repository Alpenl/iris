use super::{
    instance::{Blob, State, checked_range, next_id},
    json_error,
    ledger::Resource,
};
use crate::bounded_json::decode;
use iris_api::v2::{CHUNK_LIMIT, ErrorCode, MESSAGE_LIMIT, Role, crypto::CryptoSpec, task::Task};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use wasmtime::{Caller, Engine, Linker, Memory};

fn check_entry(caller: &Caller<'_, State>, require_ready: bool) -> Result<(), ErrorCode> {
    if require_ready && !caller.data().ready {
        return Err(ErrorCode::BadState);
    }
    caller.data().check_live()
}
fn memory(caller: &mut Caller<'_, State>) -> Result<Memory, ErrorCode> {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(ErrorCode::InvalidArgument)
}
fn read(
    caller: &mut Caller<'_, State>,
    ptr: i32,
    len: i32,
    limit: usize,
) -> Result<Vec<u8>, ErrorCode> {
    caller.data().check_live()?;
    let len = len as u32;
    if len as usize > limit {
        return Err(ErrorCode::QuotaExceeded);
    }
    let memory = memory(caller)?;
    let range = checked_range(ptr as u32, len, memory.data_size(&*caller), true)?;
    let mut bytes = vec![0; len as usize];
    memory
        .read(&*caller, range.start, &mut bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?;
    Ok(bytes)
}
fn write(caller: &mut Caller<'_, State>, ptr: i32, bytes: &[u8]) -> Result<(), ErrorCode> {
    let memory = memory(caller)?;
    let range = checked_range(
        ptr as u32,
        bytes.len() as u32,
        memory.data_size(&*caller),
        true,
    )?;
    memory
        .write(&mut *caller, range.start, bytes)
        .map_err(|_| ErrorCode::InvalidArgument)
}
fn id(value: i64) -> Result<u64, ErrorCode> {
    if value <= 0 {
        Err(ErrorCode::StaleHandle)
    } else {
        Ok(value as u64)
    }
}
fn finish(result: Result<u64, ErrorCode>) -> i64 {
    result.map(|v| v as i64).unwrap_or_else(ErrorCode::as_i64)
}
fn scratch(
    caller: &Caller<'_, State>,
    bytes: usize,
) -> Result<super::ledger::Reservation, ErrorCode> {
    caller
        .data()
        .ledger
        .reserve(&caller.data().owner, &[(Resource::HostBytes, bytes as u64)])
}

pub(crate) fn linker(engine: &Engine) -> Result<Linker<State>, ErrorCode> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap(
            "iris_v2",
            "host_submit",
            |mut caller: Caller<'_, State>, ptr: i32, len: i32| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let _scratch = scratch(&caller, MESSAGE_LIMIT * 32)?;
                    let bytes = read(&mut caller, ptr, len, MESSAGE_LIMIT)?;
                    let task: Task = decode(&bytes, MESSAGE_LIMIT).map_err(json_error)?;
                    if !task.authorized(&caller.data().grants)
                        || (caller.data().role == Role::Stream && !task.allowed_in_stream())
                    {
                        return Err(ErrorCode::Denied);
                    }
                    let state = caller.data_mut();
                    state
                        .services
                        .as_mut()
                        .ok_or(ErrorCode::Denied)?
                        .submit(task, &state.blobs)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_blob_new",
            |mut caller: Caller<'_, State>, ptr: i32, len: i32| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let _scratch = scratch(&caller, CHUNK_LIMIT)?;
                    let handle = caller
                        .data()
                        .ledger
                        .reserve(&caller.data().owner, &[(Resource::Handle, 1)])?;
                    let bytes = read(&mut caller, ptr, len, CHUNK_LIMIT)?;
                    let blob = Blob::new(&caller.data().ledger, &caller.data().owner, bytes)?;
                    let id = next_id()?;
                    caller.data_mut().blobs.insert(id, Arc::new(blob));
                    caller.data_mut().blob_handles.insert(id, handle);
                    Ok(id)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_blob_len",
            |caller: Caller<'_, State>, handle: i64| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let blob = caller
                        .data()
                        .blobs
                        .get(&id(handle)?)
                        .ok_or(ErrorCode::StaleHandle)?;
                    Ok(blob.bytes.len() as u64)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_blob_read",
            |mut caller: Caller<'_, State>, handle: i64, offset: i64, dst: i32, cap: i32| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    if cap as u32 == 0 || cap as u32 as usize > CHUNK_LIMIT || offset < 0 {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let blob = caller
                        .data()
                        .blobs
                        .get(&id(handle)?)
                        .cloned()
                        .ok_or(ErrorCode::StaleHandle)?;
                    let offset = usize::try_from(offset).map_err(|_| ErrorCode::RangeInvalid)?;
                    if offset > blob.bytes.len() {
                        return Err(ErrorCode::RangeInvalid);
                    }
                    // Validate the full destination capacity, including EOF calls.
                    let memory = memory(&mut caller)?;
                    checked_range(dst as u32, cap as u32, memory.data_size(&caller), false)?;
                    let count = (blob.bytes.len() - offset).min(cap as usize);
                    write(&mut caller, dst, &blob.bytes[offset..offset + count])?;
                    Ok(count as u64)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_close",
            |mut caller: Caller<'_, State>, handle: i64| -> i32 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let id = id(handle)?;
                    if caller.data_mut().blobs.remove(&id).is_none() {
                        caller
                            .data_mut()
                            .services
                            .as_mut()
                            .ok_or(ErrorCode::StaleHandle)?
                            .close(id)?;
                    }
                    caller.data_mut().blob_handles.remove(&id);
                    Ok(0)
                })()) as i32
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_stream_reply",
            |mut caller: Caller<'_, State>, request: i64, blob: i64, flags: i32| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    if caller.data().role != Role::Stream {
                        return Err(ErrorCode::Denied);
                    }
                    if flags & !1 != 0 {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let blob_id = id(blob)?;
                    let blob = caller
                        .data()
                        .blobs
                        .get(&blob_id)
                        .cloned()
                        .ok_or(ErrorCode::StaleHandle)?;
                    let count = caller
                        .data_mut()
                        .services
                        .as_mut()
                        .ok_or(ErrorCode::StaleHandle)?
                        .reply(id(request)?, blob, flags == 1)?;
                    caller.data_mut().blobs.remove(&blob_id);
                    caller.data_mut().blob_handles.remove(&blob_id);
                    Ok(count as u64)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_stream_fail",
            |mut caller: Caller<'_, State>, request: i64, code: i32| -> i32 {
                finish((|| {
                    check_entry(&caller, true)?;
                    if caller.data().role != Role::Stream {
                        return Err(ErrorCode::Denied);
                    }
                    let error = ErrorCode::from_i32(code).ok_or(ErrorCode::InvalidArgument)?;
                    caller
                        .data_mut()
                        .services
                        .as_mut()
                        .ok_or(ErrorCode::StaleHandle)?
                        .fail(id(request)?, error)?;
                    Ok(0)
                })()) as i32
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_crypto",
            |mut caller: Caller<'_, State>,
             spec_ptr: i32,
             spec_len: i32,
             in_ptr: i32,
             in_len: i32,
             out_ptr: i32,
             out_cap: i32|
             -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let _scratch = scratch(&caller, CHUNK_LIMIT * 8)?;
                    if out_cap as u32 as usize > CHUNK_LIMIT + 16 {
                        return Err(ErrorCode::QuotaExceeded);
                    }
                    let spec: CryptoSpec =
                        decode(&read(&mut caller, spec_ptr, spec_len, 4096)?, 4096)
                            .map_err(json_error)?;
                    let memory = memory(&mut caller)?;
                    let a = checked_range(
                        in_ptr as u32,
                        in_len as u32,
                        memory.data_size(&caller),
                        true,
                    )?;
                    let b = checked_range(
                        out_ptr as u32,
                        out_cap as u32,
                        memory.data_size(&caller),
                        true,
                    )?;
                    if a.start != b.start && a.start < b.end && b.start < a.end {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    let input = read(&mut caller, in_ptr, in_len, CHUNK_LIMIT)?;
                    let output =
                        super::crypto::compute(&spec, &input, out_cap as u32 as usize, |id| {
                            caller
                                .data()
                                .blobs
                                .get(&id)
                                .map(|b| b.bytes.clone())
                                .ok_or(ErrorCode::StaleHandle)
                        })?;
                    write(&mut caller, out_ptr, &output)?;
                    Ok(output.len() as u64)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_random",
            |mut caller: Caller<'_, State>, dst: i32, len: i32| -> i32 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let _scratch = scratch(&caller, MESSAGE_LIMIT)?;
                    if len as u32 as usize > MESSAGE_LIMIT {
                        return Err(ErrorCode::QuotaExceeded);
                    }
                    let mut bytes = vec![0; len as u32 as usize];
                    SystemRandom::new()
                        .fill(&mut bytes)
                        .map_err(|_| ErrorCode::Internal)?;
                    write(&mut caller, dst, &bytes)?;
                    Ok(0)
                })()) as i32
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    linker
        .func_wrap(
            "iris_v2",
            "host_clock",
            |caller: Caller<'_, State>, kind: i32| -> i64 {
                finish((|| {
                    check_entry(&caller, true)?;
                    let elapsed = match kind {
                        0 => SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| ErrorCode::Internal)?,
                        1 => caller.data().started.elapsed(),
                        _ => return Err(ErrorCode::Unsupported),
                    };
                    u64::try_from(elapsed.as_millis())
                        .ok()
                        .filter(|&n| n <= i64::MAX as u64)
                        .ok_or(ErrorCode::Internal)
                })())
            },
        )
        .map_err(|_| ErrorCode::Internal)?;
    Ok(linker)
}
