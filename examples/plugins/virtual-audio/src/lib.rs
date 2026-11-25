//! Three seconds of quiet 400 Hz PCM supplied by an isolated StreamStore.
pub const SIZE: usize = 44 + 8000 * 3 * 2;
pub fn read(offset: usize, len: usize) -> Option<Vec<u8>> {
    if offset > SIZE || len == 0 || len > iris_plugin_sdk::api::v2::CHUNK_LIMIT {
        return None;
    }
    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&((SIZE - 8) as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&8000u32.to_le_bytes());
    header.extend_from_slice(&16000u32.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&((SIZE - 44) as u32).to_le_bytes());
    Some(
        (offset..SIZE.min(offset.saturating_add(len)))
            .map(|i| {
                if i < 44 {
                    header[i]
                } else {
                    let n = (i - 44) / 2;
                    let value = if n % 20 < 10 { 1000i16 } else { -1000 };
                    value.to_le_bytes()[(i - 44) % 2]
                }
            })
            .collect(),
    )
}
#[cfg(target_arch = "wasm32")]
mod wasm {
    use iris_plugin_sdk::{
        api::v2::{ErrorCode, GuestError, HostCommand, HostEvent, Reply},
        guest,
        serde_json::{self, Value, json},
    };
    fn handle(event: HostEvent<Value>) -> Result<Vec<HostCommand>, ErrorCode> {
        match event {
            HostEvent::Window { body, .. } => handle(*body),
            HostEvent::SourceBrowse {
                request_id,
                source_id,
                folder_id,
                query,
                ..
            } => {
                let entries = if query.as_ref().is_none_or(|q| "测试音频".contains(q)) {
                    json!([{"id":"tone","name":"测试音频（3 秒）","kind":"audio","size":super::SIZE.to_string(),"duration":3.0}])
                } else {
                    json!([])
                };
                Ok(vec![serde_json::from_value(json!({"command":"provide_source_page","request_id":request_id,"page":{"source_id":source_id,"folder_id":folder_id,"parent_id":null,"title":"虚拟 PCM 音频","entries":entries,"next_page":null}})).map_err(|_|ErrorCode::Internal)?])
            }
            HostEvent::SourceResolve {
                request_id,
                entry_id,
                ..
            } if entry_id == "tone" => {
                let context = guest::Blob::new(b"pcm-v1")?.transfer();
                Ok(vec![serde_json::from_value(json!({"command":"resolve_stream","request_id":request_id,"media":{"stream":{"source_id":"audio","entry_id":"tone","title":"虚拟 PCM 音频","kind":"audio","source":{"mode":"random_access","resource_id":"tone","size":super::SIZE.to_string(),"context":{"version":"1","blob":context}}}}})).map_err(|_|ErrorCode::Internal)?])
            }
            HostEvent::StreamInit { context_blob, .. } => {
                drop(guest::Blob::from_completion(context_blob)?);
                Ok(Vec::new())
            }
            HostEvent::StreamRead {
                request_id,
                resource_id,
                offset,
                len,
                ..
            } => {
                if resource_id != "tone" {
                    guest::fail_stream(request_id, ErrorCode::Denied)?;
                    return Ok(Vec::new());
                }
                let start = usize::try_from(offset.0).map_err(|_| ErrorCode::RangeInvalid)?;
                match super::read(start, len as usize) {
                    Some(bytes) => {
                        let eof = start + bytes.len() == super::SIZE;
                        guest::Blob::new(&bytes)?.reply(request_id, eof)?;
                    }
                    None => guest::fail_stream(request_id, ErrorCode::RangeInvalid)?,
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }
    fn dispatch(value: Value) -> Reply<HostCommand> {
        match serde_json::from_value(value)
            .map_err(|_| ErrorCode::InvalidArgument)
            .and_then(handle)
        {
            Ok(ok) => Reply::Ok { ok },
            Err(code) => Reply::Error {
                error: GuestError {
                    code: code as i32,
                    message: format!("{code:?}"),
                },
            },
        }
    }
    iris_plugin_sdk::export_plugin!(dispatch);
}
#[cfg(test)]
mod tests {
    #[test]
    fn unaligned_and_eof_reads_match_whole_pcm_file() {
        let full = super::read(0, super::SIZE).unwrap();
        assert_eq!(&full[..4], b"RIFF");
        for offset in [0, 1, 43, 44, 45, super::SIZE - 1, super::SIZE] {
            assert_eq!(
                super::read(offset, 7).unwrap(),
                full[offset..full.len().min(offset + 7)]
            );
        }
        assert!(super::read(super::SIZE + 1, 1).is_none());
    }
}
