//! Three seconds of quiet 400 Hz PCM supplied by an isolated StreamStore.
pub const SIZE: usize = 44 + 8000 * 3 * 2;
pub const ENTRY_NAME: &str = "测试音频（3 秒）";

pub fn matches_query(query: Option<&str>) -> bool {
    query
        .map(str::trim)
        .is_none_or(|q| q.is_empty() || ENTRY_NAME.contains(q))
}

pub fn resolve_error(
    source_id: &str,
    entry_id: &str,
    variant: Option<&str>,
) -> Option<&'static str> {
    if source_id != "audio" || entry_id != "tone" {
        Some("未找到请求的虚拟音频条目")
    } else if variant.is_some() {
        Some("虚拟音频不支持清晰度或线路切换")
    } else {
        None
    }
}

fn wav_header() -> [u8; 44] {
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&((SIZE - 8) as u32).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&1u16.to_le_bytes());
    header[24..28].copy_from_slice(&8000u32.to_le_bytes());
    header[28..32].copy_from_slice(&16000u32.to_le_bytes());
    header[32..34].copy_from_slice(&2u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&((SIZE - 44) as u32).to_le_bytes());
    header
}

pub fn read(offset: usize, len: usize) -> Option<Vec<u8>> {
    if offset > SIZE || len == 0 || len > iris_plugin_sdk::api::v2::CHUNK_LIMIT {
        return None;
    }
    let end = SIZE.min(offset.saturating_add(len));
    let mut out = Vec::with_capacity(end - offset);
    if offset < 44 {
        let header = wav_header();
        out.extend_from_slice(&header[offset..end.min(44)]);
    }
    for i in offset.max(44)..end {
        let n = (i - 44) / 2;
        let value = if n % 20 < 10 { 1000i16 } else { -1000 };
        out.push(value.to_le_bytes()[(i - 44) % 2]);
    }
    Some(out)
}
pub fn read_at(offset: u64, len: u32) -> Option<Vec<u8>> {
    let start = usize::try_from(offset).ok()?;
    read(start, len as usize)
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use iris_plugin_sdk::{
        api::v2::{ErrorCode, HostCommand, HostEvent},
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
                let entries = if super::matches_query(query.as_deref()) {
                    json!([{"id":"tone","name":super::ENTRY_NAME,"kind":"audio","size":super::SIZE.to_string(),"duration":3.0}])
                } else {
                    json!([])
                };
                Ok(vec![serde_json::from_value(json!({"command":"provide_source_page","request_id":request_id,"page":{"source_id":source_id,"folder_id":folder_id,"parent_id":null,"title":"虚拟 PCM 音频","entries":entries,"next_page":null}})).map_err(|_|ErrorCode::Internal)?])
            }
            HostEvent::SourceResolve {
                request_id,
                source_id,
                entry_id,
                variant,
                ..
            } => {
                if let Some(message) =
                    super::resolve_error(&source_id, &entry_id, variant.as_deref())
                {
                    return Ok(vec![HostCommand::SourceError {
                        request_id,
                        message: message.into(),
                    }]);
                }
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
                let Ok(start) = usize::try_from(offset.0) else {
                    guest::fail_stream(request_id, ErrorCode::RangeInvalid)?;
                    return Ok(Vec::new());
                };
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
    iris_plugin_sdk::export_typed_plugin!(handle);
}
#[cfg(test)]
mod tests {
    use super::{SIZE, matches_query, read, resolve_error};

    #[test]
    fn unaligned_and_eof_reads_match_whole_pcm_file() {
        let full = read(0, SIZE).unwrap();
        assert_eq!(full.len(), SIZE);
        assert_eq!(&full[..4], b"RIFF");
        assert_eq!(&full[8..16], b"WAVEfmt ");
        assert_eq!(&full[36..40], b"data");
        for offset in [0, 1, 40, 43, 44, 45, 1000, SIZE - 1, SIZE] {
            assert_eq!(
                read(offset, 10).unwrap(),
                full[offset..full.len().min(offset + 10)]
            );
        }
        assert!(read(0, 0).is_none());
        assert!(read(SIZE + 1, 1).is_none());
        assert!(read(0, iris_plugin_sdk::api::v2::CHUNK_LIMIT + 1).is_none());
        assert_eq!(super::read_at(0, 4).as_deref(), Some(&b"RIFF"[..]));
        assert!(super::read_at(u64::from(u32::MAX) + 1, 16).is_none());
        assert!(super::read_at(u64::MAX, 16).is_none());
    }

    #[test]
    fn query_matching_and_resolve_validation_cover_full_entry_title_and_errors() {
        assert!(matches_query(None));
        assert!(matches_query(Some("  ")));
        assert!(matches_query(Some("测试音频")));
        assert!(matches_query(Some("3 秒")));
        assert!(!matches_query(Some("不存在")));

        assert_eq!(resolve_error("audio", "tone", None), None);
        assert!(resolve_error("audio", "unknown", None).is_some());
        assert!(resolve_error("other", "tone", None).is_some());
        assert!(resolve_error("audio", "tone", Some("1080p")).is_some());
    }
}
