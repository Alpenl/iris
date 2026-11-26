//! Validate a complete package before allowing compilation or installation.
use iris_api::v2::{ErrorCode, MESSAGE_LIMIT, Manifest};
use ring::digest::{Context, SHA256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    path::Path,
};

pub const PACKAGE_LIMIT: usize = 32 << 20;
pub const EXPANDED_LIMIT: usize = 64 << 20;
pub const WASM_LIMIT: usize = 16 << 20;
pub const ENTRY_LIMIT: usize = 1024;

pub struct Package {
    pub manifest: Manifest,
    pub digest: [u8; 32],
    files: BTreeMap<String, Vec<u8>>,
    _reservation: Option<super::ledger::Reservation>,
}

#[derive(Clone, Copy)]
pub enum Provenance<'a> {
    User,
    /// Constructed from the bundled index, never from package contents.
    Bundled(&'a BTreeMap<String, [u8; 32]>),
}

pub fn validate_path(name: &str) -> Result<(), ErrorCode> {
    if name.is_empty() || name.len() > 240 || !name.is_ascii() {
        return Err(ErrorCode::InvalidArgument);
    }
    for part in name.trim_end_matches('/').split('/') {
        let stem = part.split('.').next().unwrap_or("");
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with('.')
            || !part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            || !crate::plugin_v2::package::valid_stem(stem)
        {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(())
}

fn valid_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

impl Package {
    pub fn account(
        mut self,
        ledger: &super::ledger::Ledger,
        owner: &str,
    ) -> Result<Self, ErrorCode> {
        let bytes = self
            .files
            .iter()
            .map(|(name, bytes)| name.capacity() + bytes.capacity() + 512)
            .sum::<usize>()
            + MESSAGE_LIMIT * 32;
        self._reservation =
            Some(ledger.reserve(owner, &[(super::ledger::Resource::HostBytes, bytes as u64)])?);
        Ok(self)
    }

    pub fn from_zip(bytes: &[u8], provenance: Provenance<'_>) -> Result<Self, ErrorCode> {
        if bytes.len() > PACKAGE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        preflight_zip(bytes)?;
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| ErrorCode::InvalidArgument)?;
        if archive.len() > ENTRY_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut files = BTreeMap::new();
        let mut names = BTreeSet::new();
        let mut total = 0usize;
        for index in 0..archive.len() {
            let mut file = archive
                .by_index(index)
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let name = std::str::from_utf8(file.name_raw())
                .map_err(|_| ErrorCode::InvalidArgument)?
                .to_owned();
            validate_path(&name)?;
            let normalized = name.trim_end_matches('/').to_ascii_lowercase();
            if !names.insert(normalized) {
                return Err(ErrorCode::InvalidArgument);
            }
            if !matches!(
                file.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            ) || file
                .unix_mode()
                .is_some_and(|m| !matches!(m & 0o170000, 0 | 0o100000 | 0o040000))
            {
                return Err(ErrorCode::Unsupported);
            }
            if name.ends_with('/') {
                if file.size() != 0 {
                    return Err(ErrorCode::InvalidArgument);
                }
                continue;
            }
            if file.unix_mode().is_some_and(|m| m & 0o170000 == 0o040000) {
                return Err(ErrorCode::InvalidArgument);
            }
            let limit = file_limit(&name).min(EXPANDED_LIMIT - total);
            if file.size() > limit as u64 {
                return Err(ErrorCode::QuotaExceeded);
            }
            // Probe EOF/CRC without growing an already full output buffer.
            let mut data = vec![0; file.size() as usize];
            file.read_exact(&mut data)
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let mut extra = [0];
            if file
                .read(&mut extra)
                .map_err(|_| ErrorCode::InvalidArgument)?
                != 0
            {
                return Err(ErrorCode::InvalidArgument);
            }
            total += data.len();
            files.insert(name, data);
        }
        Self::from_files(files, provenance)
    }

    pub fn read_zip(path: &Path, provenance: Provenance<'_>) -> Result<Self, ErrorCode> {
        let file = std::fs::File::open(path).map_err(|_| ErrorCode::IoError)?;
        let bytes = read_bounded(file, PACKAGE_LIMIT)?;
        Self::from_zip(&bytes, provenance)
    }

    fn from_files(
        files: BTreeMap<String, Vec<u8>>,
        provenance: Provenance<'_>,
    ) -> Result<Self, ErrorCode> {
        // Reject file/directory prefix collisions even on case-insensitive hosts.
        let folded: BTreeSet<_> = files.keys().map(|s| s.to_ascii_lowercase()).collect();
        for name in files.keys() {
            let mut path = name.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                if folded.contains(&parent.to_ascii_lowercase()) {
                    return Err(ErrorCode::InvalidArgument);
                }
                path = parent;
            }
            if name != "plugin.json" && name != "plugin.wasm" && !name.starts_with("assets/") {
                return Err(ErrorCode::Unsupported);
            }
        }
        let manifest: Manifest = crate::bounded_json::decode_exact(
            files.get("plugin.json").ok_or(ErrorCode::InvalidArgument)?,
            MESSAGE_LIMIT,
        )
        .map_err(super::json_error)?;
        manifest
            .validate()
            .map_err(|_| ErrorCode::InvalidArgument)?;
        super::validation::manifest(&manifest)?;
        if manifest.entry.is_some() != files.contains_key("plugin.wasm") {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut hash = Context::new(&SHA256);
        for (path, bytes) in &files {
            hash.update(&(path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let digest: [u8; 32] = hash
            .finish()
            .as_ref()
            .try_into()
            .map_err(|_| ErrorCode::Internal)?;
        if manifest.id.to_ascii_lowercase().starts_with("iris.") {
            match provenance {
                Provenance::Bundled(index) if index.get(&manifest.id) == Some(&digest) => {}
                _ => return Err(ErrorCode::Denied),
            }
        }
        Ok(Self {
            manifest,
            digest,
            files,
            _reservation: None,
        })
    }

    pub fn wasm(&self) -> Option<&[u8]> {
        (self.manifest.entry.as_deref() == Some("plugin.wasm"))
            .then(|| self.files.get("plugin.wasm").map(Vec::as_slice))
            .flatten()
    }
}

fn file_limit(name: &str) -> usize {
    match name {
        "plugin.json" => MESSAGE_LIMIT,
        "plugin.wasm" => WASM_LIMIT,
        _ => EXPANDED_LIMIT,
    }
}

pub(crate) fn read_bounded(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, ErrorCode> {
    let mut bytes = Vec::new();
    let mut block = [0; 8192];
    loop {
        let len = reader.read(&mut block).map_err(|_| ErrorCode::IoError)?;
        if len == 0 {
            break;
        }
        let needed = bytes
            .len()
            .checked_add(len)
            .ok_or(ErrorCode::QuotaExceeded)?;
        if needed > limit {
            return Err(ErrorCode::QuotaExceeded);
        }
        if needed > bytes.capacity() {
            let capacity = bytes.capacity().saturating_mul(2).max(needed).min(limit);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|_| ErrorCode::QuotaExceeded)?;
        }
        bytes.extend_from_slice(&block[..len]);
    }
    Ok(bytes.into_boxed_slice().into_vec())
}

fn preflight_zip(bytes: &[u8]) -> Result<(), ErrorCode> {
    // ZIP64/multi-disk are unnecessary within these limits. Check the central
    // directory count before the ZIP decoder allocates entry metadata.
    let lower = bytes.len().saturating_sub(65535 + 22);
    for pos in (lower..bytes.len().saturating_sub(21)).rev() {
        if bytes.get(pos..pos + 4) != Some(b"PK\x05\x06") {
            continue;
        }
        let u16_at = |off| u16::from_le_bytes([bytes[pos + off], bytes[pos + off + 1]]);
        if pos + 22 + usize::from(u16_at(20)) != bytes.len() {
            continue;
        }
        if pos >= 20 && bytes.get(pos - 20..pos - 16) == Some(b"PK\x06\x07") {
            return Err(ErrorCode::Unsupported);
        }
        if u16_at(4) != 0 || u16_at(6) != 0 || u16_at(8) != u16_at(10) {
            return Err(ErrorCode::Unsupported);
        }
        if usize::from(u16_at(10)) > ENTRY_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        let u32_at =
            |off| u32::from_le_bytes(bytes[pos + off..pos + off + 4].try_into().unwrap()) as usize;
        let size = u32_at(12);
        let start = u32_at(16);
        if size > 4 << 20 {
            return Err(ErrorCode::QuotaExceeded);
        }
        if start.checked_add(size) != Some(pos) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut cursor = start;
        for _ in 0..u16_at(10) {
            let header = bytes
                .get(cursor..cursor.saturating_add(46))
                .ok_or(ErrorCode::InvalidArgument)?;
            if header.get(..4) != Some(b"PK\x01\x02") {
                return Err(ErrorCode::InvalidArgument);
            }
            let len = |off| usize::from(u16::from_le_bytes([header[off], header[off + 1]]));
            cursor = cursor
                .checked_add(46 + len(28) + len(30) + len(32))
                .filter(|end| *end <= pos)
                .ok_or(ErrorCode::InvalidArgument)?;
        }
        if cursor != pos {
            return Err(ErrorCode::InvalidArgument);
        }
        return Ok(());
    }
    Err(ErrorCode::InvalidArgument)
}

#[cfg(test)]
mod tests;
