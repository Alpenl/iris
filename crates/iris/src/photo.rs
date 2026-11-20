//! Reads what mpv gets wrong about a photo from its file header, and turns it
//! into per-file `loadfile` options.
//!
//! Measured on mpv 0.42 / FFmpeg (docs/TESTING.md):
//! - embedded colour profiles are ignored (see `icc`);
//! - HEIF rotation and mirroring (`irot` / `imir`) are ignored, so portrait
//!   iPhone photos show sideways. JPEG EXIF orientation already works;
//! - images whose long side is past the GPU texture limit fail to display.
//!
//! Each reader looks only at the header and stops at the image data, so the
//! cost is a few small reads, not a decode.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::icc::{self, Colour, be16, be32};

/// Profiles bigger than this are not photo profiles we can match anyway.
const MAX_ICC: usize = 4 << 20;

/// Longest side handed to mpv's GPU renderer. The texture limit is 16384,
/// but 12000×12000 already logs texture/framebuffer failures and uses
/// ~760 MB; 8192 leaves 48 MP photos (8064×6048) untouched.
const MAX_SIDE: u32 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    /// Left-right (`hflip`).
    Horizontal,
    /// Top-bottom (`vflip`).
    Vertical,
}

/// What a photo's header says.
#[derive(Debug, Default, PartialEq)]
pub struct Photo {
    pub colour: Option<Colour>,
    /// Stored pixel size, before any rotation.
    pub size: Option<(u32, u32)>,
    /// HEIF only (mpv handles JPEG EXIF itself). Applied as: mirror first
    /// (a pixel filter), then rotate clockwise (in the renderer).
    pub mirror: Option<Mirror>,
    pub rotate_cw: u16,
}

impl Photo {
    /// The `loadfile` per-file options, or `None` when mpv's defaults are
    /// right. mpv drops them again when the file ends.
    pub fn mpv_options(&self) -> Option<String> {
        let mut vf = Vec::new();
        // Scale first, so later filters work on the small image.
        if let Some((w, h)) = self.size
            && w.max(h) > MAX_SIDE
        {
            // -2 keeps the aspect ratio with an even size (chroma subsampled
            // formats need one).
            vf.push(if w >= h {
                format!("scale=w={MAX_SIDE}:h=-2")
            } else {
                format!("scale=w=-2:h={MAX_SIDE}")
            });
        }
        match self.mirror {
            Some(Mirror::Horizontal) => vf.push("hflip".into()),
            Some(Mirror::Vertical) => vf.push("vflip".into()),
            None => {}
        }
        // mpv's own `format` filter tags the image; it goes last, so no
        // lavfi filter in front can drop the tags. Rotation is a tag too:
        // the GPU renderer turns the image, no CPU pass. (`video-rotate`
        // does nothing for tiled HEIC, which mpv stitches in a separate
        // filter graph.)
        let colour = self.colour.and_then(|c| c.mpv_format());
        let clip = colour.is_some();
        let mut format: Vec<String> = colour.into_iter().collect();
        if self.rotate_cw != 0 {
            format.push(format!("rotate={}", self.rotate_cw));
        }
        if !format.is_empty() {
            vf.push(format!("format={}", format.join(":")));
        }

        let mut opts = Vec::new();
        if !vf.is_empty() {
            // The option list is comma separated, and so is a filter chain:
            // `%len%` quotes the value for mpv.
            let chain = vf.join(",");
            opts.push(format!("vf=%{}%{chain}", chain.len()));
        }
        if clip {
            // Clip colours outside the screen's gamut, as ColorSync does for
            // photos (relative colorimetric). mpv's default compresses them,
            // shifting e.g. Adobe RGB green towards cyan. Videos keep it.
            opts.push("gamut-mapping-mode=clip".into());
        }
        (!opts.is_empty()).then(|| opts.join(","))
    }
}

/// Reads a photo's header. Unknown formats and unreadable files give the
/// default (no overrides).
pub fn probe(path: &Path) -> Photo {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Photo::default();
    };
    let Ok(file) = File::open(path) else {
        return Photo::default();
    };
    let mut r = BufReader::new(file);
    let mut h = Header::default();
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => jpeg(&mut r, &mut h),
        "png" => png(&mut r, &mut h),
        "webp" => webp(&mut r, &mut h),
        "heic" | "heif" | "avif" => heif(&mut r, &mut h),
        "tif" | "tiff" => tiff(&mut r, &mut h),
        _ => None,
    };
    let colour = match h.colour {
        Some(ColourTag::Icc(icc)) => icc::classify_icc(&icc),
        Some(ColourTag::Nclx {
            primaries,
            transfer,
        }) => icc::classify_nclx(primaries, transfer),
        None => None,
    };
    Photo {
        colour,
        size: h.size,
        mirror: h.mirror,
        rotate_cw: h.rotate_cw,
    }
}

/// Fields collected while walking a header; readers fill what they find and
/// return `None` to stop early.
#[derive(Default)]
struct Header {
    colour: Option<ColourTag>,
    size: Option<(u32, u32)>,
    mirror: Option<Mirror>,
    rotate_cw: u16,
}

enum ColourTag {
    Icc(Vec<u8>),
    /// ISO/IEC 23091-2 code points from a HEIF `colr` box.
    Nclx {
        primaries: u16,
        transfer: u16,
    },
}

fn read_n<R: Read>(r: &mut R, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0; n];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// JPEG: the profile is split over APP2 segments tagged `ICC_PROFILE\0`; the
/// size is in the SOF segment. Both come before the scan data.
fn jpeg<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    if read_n(r, 2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut total = 0;
    loop {
        let mut m = [0u8; 2];
        if r.read_exact(&mut m).is_err() || m[0] != 0xFF {
            break;
        }
        let marker = m[1];
        match marker {
            0xFF => {
                // Fill byte: the real marker follows.
                r.seek(SeekFrom::Current(-1)).ok()?;
                continue;
            }
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xDA | 0xD9 => break, // start of scan / end of image
            _ => {}
        }
        let len = be16(&read_n(r, 2)?) as usize;
        if len < 2 {
            break;
        }
        let body = len - 2;
        let sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if sof && body >= 5 {
            let seg = read_n(r, body)?;
            h.size = Some((be16(&seg[3..]) as u32, be16(&seg[1..]) as u32));
        } else if marker == 0xE2 && body > 14 {
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

/// PNG: `IHDR` first, then maybe `iCCP` (name, method, zlib data) before
/// `IDAT`.
fn png<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    if read_n(r, 8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    loop {
        let head = read_n(r, 8)?;
        let len = be32(&head) as usize;
        match &head[4..8] {
            b"IHDR" if len >= 8 => {
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
                r.seek(SeekFrom::Current(len as i64 + 4)).ok()?;
            }
        }
    }
}

/// WebP: `VP8X` (canvas size), `ICCP`, then the bitstream chunk.
fn webp<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    let head = read_n(r, 12)?;
    if &head[..4] != b"RIFF" || &head[8..12] != b"WEBP" {
        return None;
    }
    let le24 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    loop {
        let c = read_n(r, 8)?;
        let len = u32::from_le_bytes([c[4], c[5], c[6], c[7]]) as usize;
        match &c[..4] {
            b"VP8X" if len >= 10 => {
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
                r.seek(SeekFrom::Current((len + (len & 1)) as i64)).ok()?;
            }
        }
    }
}

/// HEIF/AVIF: the primary item's properties in `meta`: `colr`, `ispe`,
/// `irot`, `imir`.
fn heif<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    // Find the top-level `meta` box; it sits before `mdat` in practice, but
    // walk past whatever comes first.
    let end = r.seek(SeekFrom::End(0)).ok()?;
    r.seek(SeekFrom::Start(0)).ok()?;
    let mut pos = 0u64;
    let meta = loop {
        if pos + 8 > end {
            return None;
        }
        let (kind, header, size) = box_header(r, end - pos)?;
        if &kind == b"meta" {
            let body = size.checked_sub(header)? as usize;
            if body > 16 << 20 {
                return None;
            }
            break read_n(r, body)?;
        }
        pos += size;
        r.seek(SeekFrom::Start(pos)).ok()?;
    };
    // `meta` is a full box: skip version and flags.
    let meta = meta.get(4..)?;
    let iprp = find_box(meta, b"iprp")?;
    let props: Vec<([u8; 4], &[u8])> = boxes(find_box(iprp, b"ipco")?).collect();
    let primary = find_box(meta, b"pitm").and_then(|b| match b.first()? {
        0 => b.get(4..6).map(|v| be16(v) as u32),
        _ => b.get(4..8).map(be32),
    });
    // Properties of the primary item, in association order (transforms
    // apply in that order). Without `ipma`, fall back to all properties.
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
                // Prefer an ICC profile over nclx when both are present: it
                // is what ImageIO and browsers use for RGB-coded photos.
                b"prof" | b"rICC" if body.len() - 4 <= MAX_ICC => {
                    h.colour = Some(ColourTag::Icc(body[4..].to_vec()));
                }
                b"nclx" if body.len() >= 8 && h.colour.is_none() => {
                    h.colour = Some(ColourTag::Nclx {
                        primaries: be16(&body[4..]),
                        transfer: be16(&body[6..]),
                    });
                }
                _ => {}
            },
            // Full box: version/flags, then width and height.
            b"ispe" if body.len() >= 12 => h.size = Some((be32(&body[4..]), be32(&body[8..]))),
            // Anticlockwise quarter turns in the low two bits.
            b"irot" if !body.is_empty() => {
                let cw = (360 - 90 * (body[0] as u16 & 3)) % 360;
                h.rotate_cw = (h.rotate_cw + cw) % 360;
            }
            // Axis in the low bit: 0 = vertical axis (left-right flip).
            b"imir" if !body.is_empty() => {
                let m = if body[0] & 1 == 0 {
                    Mirror::Horizontal
                } else {
                    Mirror::Vertical
                };
                add_mirror(h, m);
            }
            _ => {}
        }
    }
    Some(())
}

/// Appends a mirror after the current mirror-then-rotate pipeline, keeping
/// that shape: M·R(θ) = R(−θ)·M, and two different mirrors make a half turn.
fn add_mirror(h: &mut Header, m: Mirror) {
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

/// The 1-based `ipco` indices associated with `item` in an `ipma` box.
fn ipma_for(ipma: &[u8], item: u32) -> Option<Vec<usize>> {
    let version = *ipma.first()?;
    let wide_index = be32(ipma.get(..4)?) & 1 != 0;
    let mut p = 4;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = ipma.get(p..p + n)?;
        p += n;
        Some(s)
    };
    let count = be32(take(4)?);
    for _ in 0..count {
        let id = if version < 1 {
            be16(take(2)?) as u32
        } else {
            be32(take(4)?)
        };
        let n = take(1)?[0] as usize;
        let mut indices = Vec::with_capacity(n);
        for _ in 0..n {
            // The top bit marks the property essential; the rest is the index.
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

/// Reads a box header at the reader's position; returns type, header length
/// and total size.
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

/// Iterates the boxes in `buf` as (type, body).
fn boxes(mut buf: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        if buf.len() < 8 {
            return None;
        }
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        let (header, size) = match be32(buf) {
            0 => (8, buf.len()),
            1 if buf.len() >= 16 => (16, u64::from_be_bytes(buf[8..16].try_into().ok()?) as usize),
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

/// TIFF: ImageWidth (256), ImageLength (257) and InterColorProfile (34675)
/// in the first IFD.
fn tiff<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
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
    let n = u16_(&read_n(r, 2)?) as usize;
    let entries = read_n(r, n * 12)?;
    let (mut w, mut ht, mut icc_at) = (None, None, None);
    for e in entries.as_chunks::<12>().0 {
        // SHORT (3) values sit in the first two bytes of the value field.
        let value = || {
            if u16_(&e[2..]) == 3 {
                u16_(&e[8..]) as u32
            } else {
                u32_(&e[8..])
            }
        };
        match u16_(e) {
            256 => w = Some(value()),
            257 => ht = Some(value()),
            34675 => icc_at = Some((u32_(&e[8..]) as u64, u32_(&e[4..]) as usize)),
            _ => {}
        }
    }
    h.size = w.zip(ht);
    if let Some((offset, count)) = icc_at.filter(|&(_, c)| c > 4 && c <= MAX_ICC) {
        r.seek(SeekFrom::Start(offset)).ok()?;
        h.colour = Some(ColourTag::Icc(read_n(r, count)?));
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `scripts/make-photo-test-media.sh` output; tests skip without it.
    fn media(rel: &str) -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-media/photo")
            .join(rel);
        p.exists().then_some(p)
    }

    #[test]
    fn reads_colour_from_test_media() {
        let cases = [
            ("icc/red-untagged.png", None),
            ("icc/red-srgb.png", Some("bt.709")),
            ("icc/red-p3.png", Some("display-p3")),
            ("icc/red-adobergb.png", Some("adobe")),
            ("icc/red-p3-jpeg.jpg", Some("display-p3")),
        ];
        for (file, want) in cases {
            let Some(p) = media(file) else { continue };
            assert_eq!(probe(&p).colour.map(|c| c.primaries), want, "{file}");
        }
    }

    #[test]
    fn reads_size_and_heif_rotation() {
        let cases = [
            ("exif/exif-o6.jpg", (3000, 2000), 0),
            ("exif/heic-o6.heic", (3000, 2000), 90),
            ("heic/heic-48mp-8064x6048.heic", (8064, 6048), 0),
            ("huge/huge-20000x3000.png", (20000, 3000), 0),
            ("huge/huge-12000x12000.jpg", (12000, 12000), 0),
        ];
        for (file, size, rotate) in cases {
            let Some(p) = media(file) else { continue };
            let photo = probe(&p);
            assert_eq!(
                (photo.size, photo.rotate_cw),
                (Some(size), rotate),
                "{file}"
            );
        }
    }

    #[test]
    fn mpv_options() {
        assert_eq!(Photo::default().mpv_options(), None);
        let normal = Photo {
            size: Some((8064, 6048)),
            ..Default::default()
        };
        assert_eq!(normal.mpv_options(), None);
        let portrait = Photo {
            colour: Some(Colour {
                primaries: "display-p3",
                gamma: Some("srgb"),
            }),
            size: Some((4000, 40000)),
            rotate_cw: 90,
            ..Default::default()
        };
        let chain = "scale=w=-2:h=8192,format=primaries=display-p3:gamma=srgb:rotate=90";
        assert_eq!(
            portrait.mpv_options().unwrap(),
            format!("vf=%{}%{chain},gamut-mapping-mode=clip", chain.len())
        );
        let rotated = Photo {
            rotate_cw: 270,
            ..Default::default()
        };
        assert_eq!(
            rotated.mpv_options().as_deref(),
            Some("vf=%17%format=rotate=270")
        );
    }

    #[test]
    fn mirror_after_rotation_reverses_it() {
        let mut h = Header {
            rotate_cw: 90,
            ..Default::default()
        };
        add_mirror(&mut h, Mirror::Horizontal);
        assert_eq!((h.mirror, h.rotate_cw), (Some(Mirror::Horizontal), 270));
        add_mirror(&mut h, Mirror::Vertical);
        assert_eq!((h.mirror, h.rotate_cw), (None, 270));
    }
}
