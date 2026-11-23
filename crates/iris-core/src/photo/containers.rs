//! Image container header parsers (JPEG, PNG, WebP, HEIF/AVIF, TIFF) with
//! magic-byte-first format sniffing.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::Mirror;
use super::exif::{ExifSummary, parse_exif_tiff};
use super::icc::{be16, be32};

const MAX_ICC: usize = 4 << 20;
const MAX_CHUNKS: usize = 4096;
const MAX_IFD_ENTRIES: usize = 4096;

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
    if magic.starts_with(b"II*\0") || magic.starts_with(b"MM\0*") {
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
    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut total = 0;
    for _ in 0..MAX_CHUNKS {
        let mut m = [0u8; 2];
        if r.read_exact(&mut m).is_err() || m[0] != 0xFF {
            break;
        }
        let marker = m[1];
        match marker {
            0xFF => {
                r.seek(SeekFrom::Current(-1)).ok()?;
                continue;
            }
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
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
            let seg = read_n(r, body)?;
            h.size = Some((be16(&seg[3..]) as u32, be16(&seg[1..]) as u32));
        } else if marker == 0xE1
            && (15..=262_144).contains(&body)
            && h.exif_orientation.is_none()
            && h.exif.is_none()
        {
            let seg = read_n(r, body)?;
            if &seg[..6] == b"Exif\0\0" {
                let parsed = parse_exif_tiff(&seg[6..], !orientation_only);
                (h.exif_orientation, h.exif) = (parsed.orientation, parsed.summary);
                if orientation_only {
                    return Some(());
                }
            }
        } else if !orientation_only && marker == 0xE2 && body > 14 {
            let seg = read_n(r, body)?;
            if &seg[..12] == b"ICC_PROFILE\0" {
                total += seg.len() - 14;
                if total > MAX_ICC {
                    chunks.clear();
                    break;
                }
                chunks.push((seg[12], seg[14..].to_vec()));
            }
        } else {
            r.seek(SeekFrom::Current(body as i64)).ok()?;
        }
    }
    if !chunks.is_empty() {
        chunks.sort_by_key(|c| c.0);
        h.colour = Some(ColourTag::Icc(
            chunks.into_iter().flat_map(|c| c.1).collect(),
        ));
    }
    Some(())
}

pub(super) fn png<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    if read_n(r, 8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    for _ in 0..MAX_CHUNKS {
        let head = read_n(r, 8)?;
        let len_u32 = be32(&head);
        let len = len_u32 as usize;
        match &head[4..8] {
            b"IHDR" if (8..=64).contains(&len) => {
                let d = read_n(r, len)?;
                h.size = Some((be32(&d), be32(&d[4..])));
                r.seek(SeekFrom::Current(4)).ok()?;
            }
            b"iCCP" if len <= MAX_ICC => {
                let data = read_n(r, len)?;
                let name_end = data.iter().position(|&b| b == 0)?;
                let z = data.get(name_end + 2..)?;
                let icc =
                    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(z, MAX_ICC).ok()?;
                h.colour = Some(ColourTag::Icc(icc));
                return Some(());
            }
            b"IDAT" | b"IEND" => return Some(()),
            _ => {
                r.seek(SeekFrom::Current(i64::from(len_u32) + 4)).ok()?;
            }
        }
    }
    Some(())
}

pub(super) fn webp<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    let head = read_n(r, 12)?;
    if &head[..4] != b"RIFF" || &head[8..12] != b"WEBP" {
        return None;
    }
    let le24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    for _ in 0..MAX_CHUNKS {
        let c = read_n(r, 8)?;
        let len_u32 = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        let len = len_u32 as usize;
        match &c[..4] {
            b"VP8X" if (10..=1024).contains(&len) => {
                let d = read_n(r, len + (len & 1))?;
                h.size = Some((le24(&d[4..]) + 1, le24(&d[7..]) + 1));
            }
            b"ICCP" if len <= MAX_ICC => {
                h.colour = Some(ColourTag::Icc(read_n(r, len)?));
                return Some(());
            }
            b"VP8L" if h.size.is_none() && len >= 5 => {
                let d = read_n(r, 5)?;
                let bits = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
                h.size = Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
                return Some(());
            }
            b"VP8 " if h.size.is_none() && len >= 10 => {
                let d = read_n(r, 10)?;
                let le14 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as u32 & 0x3FFF;
                h.size = Some((le14(&d[6..]), le14(&d[8..])));
                return Some(());
            }
            b"VP8 " | b"VP8L" | b"ANIM" => return Some(()),
            _ => {
                let skip = i64::from(len_u32) + i64::from(len_u32 & 1);
                r.seek(SeekFrom::Current(skip)).ok()?;
            }
        }
    }
    Some(())
}

pub(super) fn heif<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    let end = r.seek(SeekFrom::End(0)).ok()?;
    r.seek(SeekFrom::Start(0)).ok()?;
    let mut pos = 0u64;
    let mut found = None;
    for _ in 0..256 {
        if pos + 8 > end {
            return None;
        }
        let (kind, header, size) = box_header(r, end - pos)?;
        if &kind == b"meta" {
            let body = usize::try_from(size.checked_sub(header)?).ok()?;
            if body > 16 << 20 {
                return None;
            }
            found = Some(read_n(r, body)?);
            break;
        }
        pos = pos.checked_add(size)?;
        r.seek(SeekFrom::Start(pos)).ok()?;
    }
    let meta = found?;
    let meta = meta.get(4..)?;
    let iprp = find_box(meta, b"iprp")?;
    let props: Vec<([u8; 4], &[u8])> = boxes(find_box(iprp, b"ipco")?).collect();
    let primary = find_box(meta, b"pitm").and_then(|b| match b.first()? {
        0 => b.get(4..6).map(|v| be16(v) as u32),
        _ => b.get(4..8).map(be32),
    });
    let order: Vec<usize> = match (primary, find_box(iprp, b"ipma")) {
        (Some(id), Some(ipma)) => ipma_for(ipma, id).unwrap_or_default(),
        _ => Vec::new(),
    };
    let chosen: Vec<&([u8; 4], &[u8])> = if order.is_empty() {
        props.iter().collect()
    } else {
        order
            .iter()
            .filter_map(|&i| props.get(i.checked_sub(1)?))
            .collect()
    };
    for (kind, body) in chosen {
        match kind {
            b"colr" if body.len() >= 4 => match &body[..4] {
                b"prof" | b"rICC" if body.len() - 4 <= MAX_ICC => {
                    h.colour = Some(ColourTag::Icc(body[4..].to_vec()));
                }
                b"nclx" if body.len() >= 8 && h.colour.is_none() => {
                    let (primaries, transfer) = (be16(&body[4..]), be16(&body[6..]));
                    h.colour = Some(ColourTag::Nclx {
                        primaries,
                        transfer,
                    });
                }
                _ => {}
            },
            b"ispe" if body.len() >= 12 => h.size = Some((be32(&body[4..]), be32(&body[8..]))),
            b"irot" if !body.is_empty() => {
                h.rotate_cw = (h.rotate_cw + (360 - 90 * (body[0] as u16 & 3)) % 360) % 360;
            }
            b"imir" if !body.is_empty() => {
                add_mirror(
                    h,
                    if body[0] & 1 == 0 {
                        Mirror::Horizontal
                    } else {
                        Mirror::Vertical
                    },
                );
            }
            _ => {}
        }
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

fn ipma_for(ipma: &[u8], item: u32) -> Option<Vec<usize>> {
    let version = *ipma.first()?;
    let wide_index = be32(ipma.get(..4)?) & 1 != 0;
    let mut p = 4;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = ipma.get(p..p + n)?;
        p += n;
        Some(s)
    };
    let count = be32(take(4)?).min(4096);
    for _ in 0..count {
        let id = if version < 1 {
            be16(take(2)?) as u32
        } else {
            be32(take(4)?)
        };
        let n = take(1)?[0] as usize;
        let mut indices = Vec::with_capacity(n);
        for _ in 0..n {
            let i = if wide_index {
                (be16(take(2)?) & 0x7FFF) as usize
            } else {
                (take(1)?[0] & 0x7F) as usize
            };
            indices.push(i);
        }
        if id == item {
            return Some(indices);
        }
    }
    None
}

fn box_header<R: Read>(r: &mut R, left: u64) -> Option<([u8; 4], u64, u64)> {
    let h = read_n(r, 8)?;
    let kind = [h[4], h[5], h[6], h[7]];
    let (header, size) = match be32(&h) {
        0 => (8, left),
        1 => {
            let l = read_n(r, 8)?;
            (16, u64::from_be_bytes(l.try_into().ok()?))
        }
        s => (8, s as u64),
    };
    (size >= header && size <= left).then_some((kind, header, size))
}

fn boxes(mut buf: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if buf.len() < 8 {
            return None;
        }
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        let (header, size) = match be32(buf) {
            0 => (8, buf.len()),
            1 if buf.len() >= 16 => (
                16,
                usize::try_from(u64::from_be_bytes(buf[8..16].try_into().ok()?)).ok()?,
            ),
            1 => return None,
            s => (8, s as usize),
        };
        if size < header || size > buf.len() {
            return None;
        }
        let body = &buf[header..size];
        buf = &buf[size..];
        Some((kind, body))
    })
}

fn find_box<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(buf).find(|(k, _)| k == kind).map(|(_, b)| b)
}

/// Unified TIFF IFD0 reader (extracts orientation `0x0112`, dimensions, and ICC profile).
pub(super) fn tiff<R: Read + Seek>(
    r: &mut R,
    h: &mut Header,
    orientation_only: bool,
) -> Option<()> {
    let head = read_n(r, 8)?;
    let le = match &head[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_ = |b: &[u8]| {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            be16(b)
        }
    };
    let u32_ = |b: &[u8]| {
        if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            be32(b)
        }
    };
    r.seek(SeekFrom::Start(u32_(&head[4..]) as u64)).ok()?;
    let n = (u16_(&read_n(r, 2)?) as usize).min(MAX_IFD_ENTRIES);
    let entries = read_n(r, n * 12)?;
    let (mut w, mut ht, mut icc_at) = (None, None, None);
    for e in entries.as_chunks::<12>().0 {
        let value = || {
            if u16_(&e[2..]) == 3 {
                u16_(&e[8..]) as u32
            } else {
                u32_(&e[8..])
            }
        };
        match u16_(e) {
            0x0112 => {
                let val = u16_(&e[8..]);
                if (1..=8).contains(&val) {
                    h.exif_orientation = Some(val);
                    if orientation_only {
                        return Some(());
                    }
                }
            }
            256 if !orientation_only => w = Some(value()),
            257 if !orientation_only => ht = Some(value()),
            34675 if !orientation_only => {
                icc_at = Some((u32_(&e[8..]) as u64, u32_(&e[4..]) as usize))
            }
            _ => {}
        }
    }
    if !orientation_only {
        h.size = w.zip(ht);
        if let Some((offset, count)) = icc_at.filter(|&(_, c)| c > 4 && c <= MAX_ICC) {
            r.seek(SeekFrom::Start(offset)).ok()?;
            h.colour = Some(ColourTag::Icc(read_n(r, count)?));
        }
    }
    Some(())
}
