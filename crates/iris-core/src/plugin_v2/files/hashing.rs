use super::identity::PinnedFile;
use crate::vfs::HttpCancel;
use iris_api::v2::{ErrorCode, WireU64, task::FileHash, task_result::HashResult};
use md5::Digest;
use ring::digest;
use std::time::{Duration, Instant};

/// Streaming file hash. The caller holds a current-media authorization and the
/// FileHash/HostBytes reservations through completion; progress is coalesced.
pub fn hash_media(
    file: &PinnedFile,
    algorithm: FileHash,
    cancel: &HttpCancel,
    mut progress: impl FnMut(u64, u64),
) -> Result<HashResult, ErrorCode> {
    let mut md5 = md5::Md5::new();
    let mut sha = digest::Context::new(&digest::SHA256);
    let size = file.initial.size;
    let sparse = matches!(algorithm, FileHash::SparseSha256V1);
    let blocks = if sparse {
        const BLOCK: u64 = 1 << 20;
        sha.update(b"iris-sparse-sha256-v1\0");
        sha.update(&size.to_le_bytes());
        let mut offsets = vec![
            0,
            (size / 2).saturating_sub(BLOCK / 2),
            size.saturating_sub(BLOCK),
        ];
        offsets.sort_unstable();
        offsets.dedup();
        offsets
            .into_iter()
            .map(|offset| (offset, (size - offset).min(BLOCK)))
            .collect::<Vec<_>>()
    } else {
        vec![(0, size)]
    };
    let total: u64 = blocks.iter().map(|(_, n)| n).sum();
    let mut done = 0;
    let mut last = Instant::now();
    let mut bytes = [0; 65536];
    for (start, len) in blocks {
        if sparse {
            sha.update(&start.to_le_bytes());
            sha.update(&len.to_le_bytes());
        }
        let mut offset = start;
        while offset < start + len {
            let take = bytes.len().min((start + len - offset) as usize);
            let n = file.read_at(offset, &mut bytes[..take], cancel)?;
            if n == 0 {
                return Err(ErrorCode::IoError);
            }
            if matches!(algorithm, FileHash::FullMd5) {
                md5.update(&bytes[..n]);
            } else {
                sha.update(&bytes[..n]);
            }
            offset += n as u64;
            done += n as u64;
            if last.elapsed() >= Duration::from_millis(250) {
                progress(done, total);
                last = Instant::now();
            }
        }
    }
    file.unchanged()?;
    if cancel.is_cancelled() {
        return Err(ErrorCode::Cancelled);
    }
    progress(total, total);
    let (algorithm, hash) = match algorithm {
        FileHash::FullMd5 => ("full_md5", md5.finalize().to_vec()),
        FileHash::FullSha256 => ("full_sha256", sha.finish().as_ref().to_vec()),
        FileHash::SparseSha256V1 => ("sparse_sha256_v1", sha.finish().as_ref().to_vec()),
    };
    let digest = hash.iter().map(|b| format!("{b:02x}")).collect();
    Ok(HashResult {
        algorithm: algorithm.into(),
        digest,
        bytes: WireU64(size),
    })
}
