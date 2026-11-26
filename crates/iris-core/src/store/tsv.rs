//! Streaming TSV reader and canonical hex / legacy URI codecs shared by `store` and `recent`.

use std::io::{self, BufRead, Read};
use std::path::Path;

use crate::fs_util;
use crate::media_uri::MediaUri;

use super::{MAX_RECORD_FIELDS, MAX_RECORD_KEY};

const HEX: &[u8; 16] = b"0123456789abcdef";

// Stream the old unbounded files while limiting each row's allocation.
pub(super) fn read_tsv(path: &Path, mut row: impl FnMut(&str) -> bool) -> io::Result<(bool, bool)> {
    let file = match fs_util::retry_sharing_violation(|| std::fs::File::open(path)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((false, false)),
        Err(e) => return Err(e),
    };
    let mut reader = io::BufReader::new(file);
    let mut line = Vec::new();
    let mut scrub = false;
    const MAX_LINE: usize = MAX_RECORD_KEY * 2 + MAX_RECORD_FIELDS;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > MAX_LINE {
            scrub = true;
            if !line.ends_with(b"\n") {
                reader.skip_until(b'\n')?;
            }
        } else if let Ok(text) = std::str::from_utf8(&line) {
            scrub |= row(text.trim_end_matches(['\r', '\n']));
        } else {
            scrub = true;
        }
    }
    Ok((true, scrub))
}

pub(super) fn canonical_hex_matches(raw: &str, key: &str) -> bool {
    raw.len() == key.len() * 2
        && raw
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(key.bytes())
            .all(|(hex, byte)| *hex == [HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]])
}

pub(super) fn push_hex(bytes: &[u8], out: &mut String) {
    out.reserve(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
}

/// Decodes a hex-encoded canonical URI, legacy UTF-8 path/WebDAV URI, or legacy OS path.
pub(super) fn decode_any_media_uri(hex_text: &str) -> Option<MediaUri> {
    if hex_text.is_empty()
        || !hex_text.len().is_multiple_of(2)
        || hex_text.len() > MAX_RECORD_KEY * 2
        || !hex_text.is_ascii()
    {
        return None;
    }
    let bytes: Vec<u8> = (0..hex_text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_text[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    if !bytes.contains(&0)
        && let Ok(s) = std::str::from_utf8(&bytes)
        && !s.is_empty()
    {
        #[cfg(windows)]
        if let Some(path) = MediaUri::legacy_unc_file_uri_path(s) {
            return Some(MediaUri::from(path));
        }
        return Some(MediaUri::parse_persisted(s));
    }
    // Legacy Windows paths used UTF-16LE, including in data directories subsequently
    // moved to Unix. Decode textual URIs before treating any bytes as an opaque OS path.
    #[cfg(unix)]
    if !bytes.contains(&0) {
        use std::os::unix::ffi::OsStringExt;
        return Some(MediaUri::Local(std::ffi::OsString::from_vec(bytes).into()));
    }
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    if units.contains(&0) {
        return None;
    }
    if let Ok(text) = String::from_utf16(&units) {
        #[cfg(windows)]
        if let Some(path) = MediaUri::legacy_unc_file_uri_path(&text) {
            return Some(MediaUri::from(path));
        }
        return Some(MediaUri::parse_persisted(&text));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        Some(MediaUri::Local(
            std::ffi::OsString::from_wide(&units).into(),
        ))
    }
    #[cfg(unix)]
    {
        None
    }
}
