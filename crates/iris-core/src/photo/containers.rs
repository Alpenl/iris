//! Image container header parsers (JPEG, PNG, WebP, HEIF/AVIF, TIFF) with
//! magic-byte-first format sniffing.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::Mirror;
use super::exif::{self, ExifSummary};
use super::icc::{be16, be32};

pub(super) const MAX_ICC: usize = 4 << 20;
pub(super) const MAX_EXIF: usize = 1 << 20;
const MAX_CHUNKS: usize = 4096;
const MAX_JPEG_FILL_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PhotoFormat {
    Jpeg,
    Png,
    Webp,
    Heif,
    Tiff,
}

pub(super) fn detect_photo_format(magic: &[u8], hint: Option<&str>) -> Option<PhotoFormat> {
    if magic.starts_with(&[0xFF, 0xD8]) {
        return Some(PhotoFormat::Jpeg);
    }
    if magic.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some(PhotoFormat::Png);
    }
    if magic.len() >= 12 && &magic[..4] == b"RIFF" && &magic[8..12] == b"WEBP" {
        return Some(PhotoFormat::Webp);
    }
    if matches!(
        magic.get(..4),
        Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")
    ) {
        return Some(PhotoFormat::Tiff);
    }
    if magic.len() >= 8 && &magic[4..8] == b"ftyp" {
        return Some(PhotoFormat::Heif);
    }
    let t = hint?.trim().split(['?', '#']).next()?;
    let ext = Path::new(t)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_else(|| t.trim_start_matches('.'));
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some(PhotoFormat::Jpeg),
        "png" => Some(PhotoFormat::Png),
        "webp" => Some(PhotoFormat::Webp),
        "heic" | "heif" | "avif" => Some(PhotoFormat::Heif),
        "tif" | "tiff" => Some(PhotoFormat::Tiff),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct Header {
    pub colour: Option<ColourTag>,
    pub size: Option<(u32, u32)>,
    pub mirror: Option<Mirror>,
    pub rotate_cw: u16,
    pub exif_orientation: Option<u16>,
    pub exif: Option<ExifSummary>,
}

pub(super) enum ColourTag {
    Icc(Vec<u8>),
    Nclx { primaries: u16, transfer: u16 },
}

fn read_n<R: Read>(r: &mut R, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0; n];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Unified JPEG header reader (`orientation_only` exits immediately after APP1 EXIF).
pub(super) fn jpeg<R: Read + Seek>(
    r: &mut R,
    h: &mut Header,
    orientation_only: bool,
) -> Option<()> {
    if read_n(r, 2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut chunks: Vec<Option<Vec<u8>>> = Vec::new();
    let (mut total, mut received, mut invalid_icc) = (0, 0, false);
    let mut fill_budget = MAX_JPEG_FILL_BYTES;
    for _ in 0..MAX_CHUNKS {
        let mut m = [0u8; 2];
        if r.read_exact(&mut m).is_err() || m[0] != 0xFF {
            break;
        }
        while m[1] == 0xFF {
            if fill_budget == 0 || r.read_exact(&mut m[1..]).is_err() {
                return None;
            }
            fill_budget -= 1;
        }
        let marker = m[1];
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0x00 => break,
            0xDA | 0xD9 => break,
            _ => {}
        }
        let len = be16(&read_n(r, 2)?) as usize;
        if len < 2 {
            break;
        }
        let body = len - 2;
        if !orientation_only
            && matches!(marker, 0xC0..=0xCF)
            && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
            && body >= 5
        {
            let mut seg = [0; 5];
            r.read_exact(&mut seg).ok()?;
            h.size = Some((be16(&seg[3..]) as u32, be16(&seg[1..]) as u32));
            r.seek(SeekFrom::Current((body - 5) as i64)).ok()?;
        } else if marker == 0xE1
            && (15..=262_144).contains(&body)
            && h.exif_orientation.is_none()
            && h.exif.is_none()
        {
            let seg = read_n(r, body)?;
            if &seg[..6] == b"Exif\0\0" {
                let mode = if orientation_only {
                    exif::Mode::Orientation
                } else {
                    exif::Mode::Summary
                };
                exif::tiff(&mut std::io::Cursor::new(&seg[6..]), h, mode);
                if orientation_only {
                    return Some(());
                }
            }
        } else if !orientation_only
            && marker == 0xE2
            && body > 14
            && h.colour.is_none()
            && !invalid_icc
        {
            let mut tag = [0; 14];
            r.read_exact(&mut tag).ok()?;
            let len = body - tag.len();
            if &tag[..12] != b"ICC_PROFILE\0" {
                r.seek(SeekFrom::Current(len as i64)).ok()?;
                continue;
            }
            let (seq, count) = (usize::from(tag[12]), usize::from(tag[13]));
            if chunks.is_empty() {
                chunks.resize_with(count, || None);
            }
            if seq == 0
                || seq > count
                || count != chunks.len()
                || chunks[seq - 1].is_some()
                || len > MAX_ICC - total
            {
                invalid_icc = true;
                chunks.clear();
                r.seek(SeekFrom::Current(len as i64)).ok()?;
                continue;
            }
            let segment = read_n(r, len)?;
            if count == 1 {
                h.colour = Some(ColourTag::Icc(segment));
                continue;
            }
            chunks[seq - 1] = Some(segment);
            total += len;
            received += 1;
            if received == count {
                let mut profile = Vec::with_capacity(total);
                for part in chunks.drain(..).flatten() {
                    profile.extend_from_slice(&part);
                }
                h.colour = Some(ColourTag::Icc(profile));
            }
        } else {
            r.seek(SeekFrom::Current(body as i64)).ok()?;
        }
    }
    Some(())
}

pub(super) fn png<R: Read + Seek>(r: &mut R, h: &mut Header, orientation_only: bool) -> Option<()> {
    if read_n(r, 8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut saw_exif = false;
    for _ in 0..MAX_CHUNKS {
        let head = read_n(r, 8)?;
        let len_u32 = be32(&head);
        let len = len_u32 as usize;
        match &head[4..8] {
            b"IHDR" if !orientation_only && (8..=64).contains(&len) => {
                let d = read_n(r, len)?;
                h.size = Some((be32(&d), be32(&d[4..])));
                r.seek(SeekFrom::Current(4)).ok()?;
            }
            b"iCCP" if !orientation_only && len <= MAX_ICC && h.colour.is_none() => {
                let data = read_n(r, len)?;
                r.seek(SeekFrom::Current(4)).ok()?;
                if let Some(name_end) = data.iter().position(|&b| b == 0)
                    && let Some(z) = data.get(name_end + 2..)
                    && let Ok(icc) =
                        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(z, MAX_ICC)
                {
                    h.colour = Some(ColourTag::Icc(icc));
                }
            }
            b"eXIf" if (8..=MAX_EXIF).contains(&len) && !saw_exif => {
                saw_exif = true;
                let data = read_n(r, len)?;
                r.seek(SeekFrom::Current(4)).ok()?;
                let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
                let mode = if orientation_only {
                    exif::Mode::Orientation
                } else {
                    exif::Mode::Summary
                };
                exif::tiff(&mut std::io::Cursor::new(tiff), h, mode);
                if orientation_only {
                    return Some(());
                }
            }
            b"IDAT" | b"IEND" => return Some(()),
            _ => {
                r.seek(SeekFrom::Current(i64::from(len_u32) + 4)).ok()?;
            }
        }
    }
    Some(())
}

pub(super) fn webp<R: Read + Seek>(
    r: &mut R,
    h: &mut Header,
    orientation_only: bool,
) -> Option<()> {
    let head = read_n(r, 12)?;
    if &head[..4] != b"RIFF" || &head[8..12] != b"WEBP" {
        return None;
    }
    let le24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    let mut left = u64::from(u32::from_le_bytes(head[4..8].try_into().ok()?)).checked_sub(4)?;
    let mut saw_exif = false;
    for _ in 0..MAX_CHUNKS {
        if left < 8 {
            break;
        }
        let mut c = [0; 8];
        r.read_exact(&mut c).ok()?;
        let len = u32::from_le_bytes(c[4..].try_into().ok()?) as usize;
        let padded = len as u64 + (len & 1) as u64;
        left = left.checked_sub(8 + padded)?;
        let start = r.stream_position().ok()?;
        match &c[..4] {
            b"VP8X" if !orientation_only && len >= 10 => {
                let mut d = [0; 10];
                r.read_exact(&mut d).ok()?;
                h.size = Some((le24(&d[4..]) + 1, le24(&d[7..]) + 1));
            }
            b"ICCP" if !orientation_only && len <= MAX_ICC && h.colour.is_none() => {
                h.colour = Some(ColourTag::Icc(read_n(r, len)?))
            }
            b"EXIF" if len <= MAX_EXIF && !saw_exif => {
                saw_exif = true;
                let data = read_n(r, len)?;
                let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
                let mode = if orientation_only {
                    exif::Mode::Orientation
                } else {
                    exif::Mode::Summary
                };
                exif::tiff(&mut std::io::Cursor::new(tiff), h, mode);
                if orientation_only {
                    return Some(());
                }
            }
            b"VP8L" if !orientation_only && h.size.is_none() && len >= 5 => {
                let mut d = [0; 5];
                r.read_exact(&mut d).ok()?;
                if d[0] == 0x2F {
                    let bits = u32::from_le_bytes(d[1..].try_into().ok()?);
                    h.size = Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
                }
            }
            b"VP8 " if !orientation_only && h.size.is_none() && len >= 10 => {
                let mut d = [0; 10];
                r.read_exact(&mut d).ok()?;
                if d[0] & 1 == 0 && d[3..6] == [0x9D, 0x01, 0x2A] {
                    let le14 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as u32 & 0x3FFF;
                    h.size = Some((le14(&d[6..]), le14(&d[8..])));
                }
            }
            _ => {}
        }
        r.seek(SeekFrom::Start(start.checked_add(padded)?)).ok()?;
    }
    Some(())
}

pub(super) fn add_mirror(h: &mut Header, m: Mirror) {
    h.rotate_cw = (360 - h.rotate_cw) % 360;
    h.mirror = match h.mirror {
        None => Some(m),
        Some(prev) if prev == m => None,
        Some(_) => {
            h.rotate_cw = (h.rotate_cw + 180) % 360;
            None
        }
    };
}
