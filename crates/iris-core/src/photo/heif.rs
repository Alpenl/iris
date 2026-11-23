//! Seek through ISO BMFF boxes without loading the image or its `mdat`/`idat` container.

use std::io::{Cursor, Read, Seek, SeekFrom};

use super::Mirror;
use super::containers::{ColourTag, Header, MAX_EXIF, MAX_ICC, add_mirror};
use super::exif;
use super::icc::{be16, be32};

const MAX_BOXES: usize = 4096;
const MAX_ITEM_TABLE: usize = 1 << 20;

#[derive(Clone, Copy)]
struct Span {
    start: u64,
    end: u64,
}

impl Span {
    fn after(self, n: u64) -> Option<Self> {
        let start = self.start.checked_add(n)?;
        (start <= self.end).then_some(Self { start, ..self })
    }

    fn prefix<const N: usize>(self, r: &mut (impl Read + Seek)) -> Option<[u8; N]> {
        self.after(N as u64)?;
        r.seek(SeekFrom::Start(self.start)).ok()?;
        let mut bytes = [0; N];
        r.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    fn bytes(self, r: &mut (impl Read + Seek), limit: usize) -> Option<Vec<u8>> {
        let len = usize::try_from(self.end - self.start).ok()?;
        if len > limit {
            return None;
        }
        r.seek(SeekFrom::Start(self.start)).ok()?;
        let mut bytes = vec![0; len];
        r.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }
}

type Atom = ([u8; 4], Span);

fn next_atom(r: &mut (impl Read + Seek), rest: &mut Span) -> Option<Atom> {
    let h = rest.prefix::<8>(r)?;
    let (head, size) = match be32(&h) {
        0 => (8, rest.end - rest.start),
        1 => (16, u64::from_be_bytes(rest.after(8)?.prefix::<8>(r)?)),
        n => (8, u64::from(n)),
    };
    if size < head {
        return None;
    }
    let start = rest.start + head;
    let kind: [u8; 4] = h[4..].try_into().ok()?;
    // A prefix may end inside a container. Its complete child boxes are still usable.
    // Leaf spans must always fit, including colour profiles and EXIF extents.
    let size = if matches!(&kind, b"meta" | b"iprp" | b"ipco" | b"iinf" | b"iref") {
        size.min(rest.end - rest.start)
    } else {
        size
    };
    *rest = rest.after(size)?;
    Some((
        kind,
        Span {
            start,
            end: rest.start,
        },
    ))
}

fn children(r: &mut (impl Read + Seek), mut span: Span) -> Vec<Atom> {
    std::iter::from_fn(|| next_atom(r, &mut span))
        .take(MAX_BOXES)
        .collect()
}

fn find(atoms: &[Atom], kind: &[u8; 4]) -> Option<Span> {
    atoms.iter().find(|(k, _)| k == kind).map(|(_, span)| *span)
}

pub(super) fn probe<R: Read + Seek>(r: &mut R, h: &mut Header) -> Option<()> {
    let file = Span {
        start: 0,
        end: r.seek(SeekFrom::End(0)).ok()?,
    };
    let mut top = file;
    for _ in 0..256 {
        let (kind, body) = next_atom(r, &mut top)?;
        if &kind != b"meta" {
            continue;
        }
        let meta = children(r, body.after(4)?);
        let primary = find(&meta, b"pitm").and_then(|span| match span.prefix::<1>(r)?[0] {
            0 => Some(u32::from(be16(&span.after(4)?.prefix::<2>(r)?))),
            1 => Some(be32(&span.after(4)?.prefix::<4>(r)?)),
            _ => None,
        });
        if let Some(iprp) = find(&meta, b"iprp") {
            properties(r, iprp, primary, h);
        }
        if let Some(data) = exif_item(r, file, &meta, primary)
            && let Some(offset) = data.get(..4).map(be32)
            && let Some(tiff) = data.get(4..).and_then(|b| b.get(offset as usize..))
        {
            exif::tiff(&mut Cursor::new(tiff), h, exif::Mode::Summary);
        }
        return Some(());
    }
    None
}

fn properties(
    r: &mut (impl Read + Seek),
    iprp: Span,
    primary: Option<u32>,
    h: &mut Header,
) -> Option<()> {
    if iprp.end - iprp.start > 16 << 20 {
        return None;
    }
    let boxes = children(r, iprp);
    let props = children(r, find(&boxes, b"ipco")?);
    if let Some(id) = primary
        && boxes.iter().any(|(k, _)| k == b"ipma")
    {
        for (_, span) in boxes.iter().filter(|(k, _)| k == b"ipma") {
            if let Some(data) = span.bytes(r, MAX_ITEM_TABLE)
                && let Some(indices) = property_indices(&data, id)
            {
                for i in indices {
                    if let Some(atom) = i.checked_sub(1).and_then(|i| props.get(i)) {
                        property(r, *atom, h);
                    }
                }
                break;
            }
        }
    } else {
        for atom in props {
            property(r, atom, h);
        }
    }
    Some(())
}

fn property(r: &mut (impl Read + Seek), (kind, body): Atom, h: &mut Header) -> Option<()> {
    match &kind {
        b"colr" => match &body.prefix::<4>(r)? {
            b"prof" | b"rICC" if !matches!(h.colour, Some(ColourTag::Icc(_))) => {
                h.colour = Some(ColourTag::Icc(body.after(4)?.bytes(r, MAX_ICC)?));
            }
            b"nclx" if h.colour.is_none() => {
                let values = body.after(4)?.prefix::<4>(r)?;
                h.colour = Some(ColourTag::Nclx {
                    primaries: be16(&values),
                    transfer: be16(&values[2..]),
                });
            }
            _ => {}
        },
        b"ispe" => {
            let size = body.after(4)?.prefix::<8>(r)?;
            h.size = Some((be32(&size), be32(&size[4..])));
        }
        b"irot" => {
            h.rotate_cw = (h.rotate_cw + 360 - 90 * u16::from(body.prefix::<1>(r)?[0] & 3)) % 360
        }
        b"imir" => add_mirror(
            h,
            if body.prefix::<1>(r)?[0] & 1 == 0 {
                Mirror::Vertical
            } else {
                Mirror::Horizontal
            },
        ),
        _ => {}
    }
    Some(())
}

fn number(buf: &mut &[u8], width: usize) -> Option<u64> {
    let (raw, tail) = buf.split_at_checked(width)?;
    let mut word = [0; 8];
    word.get_mut(8usize.checked_sub(width)?..)?
        .copy_from_slice(raw);
    *buf = tail;
    Some(u64::from_be_bytes(word))
}

fn property_indices(mut buf: &[u8], id: u32) -> Option<Vec<usize>> {
    let flags = number(&mut buf, 4)?;
    let version = flags >> 24;
    if version > 1 {
        return None;
    }
    let wide = flags & 1 != 0;
    let width = if wide { 2 } else { 1 };
    for _ in 0..number(&mut buf, 4)?.min(MAX_BOXES as u64) {
        let item = number(&mut buf, if version == 0 { 2 } else { 4 })?;
        let count = number(&mut buf, 1)? as usize;
        if item == u64::from(id) {
            return (0..count)
                .map(|_| {
                    Some((number(&mut buf, width)? as usize) & if wide { 0x7fff } else { 0x7f })
                })
                .collect();
        }
        buf = buf.get(count * width..)?;
    }
    None
}

fn exif_item(
    r: &mut (impl Read + Seek),
    file: Span,
    meta: &[Atom],
    primary: Option<u32>,
) -> Option<Vec<u8>> {
    let iinf = find(meta, b"iinf")?;
    let version = iinf.prefix::<1>(r)?[0];
    let (count, mut rest) = if version == 0 {
        (
            usize::from(be16(&iinf.after(4)?.prefix::<2>(r)?)),
            iinf.after(6)?,
        )
    } else {
        (
            be32(&iinf.after(4)?.prefix::<4>(r)?) as usize,
            iinf.after(8)?,
        )
    };
    let mut ids = Vec::new();
    for _ in 0..count.min(MAX_BOXES) {
        let Some((kind, info)) = next_atom(r, &mut rest) else {
            break;
        };
        if &kind != b"infe" {
            continue;
        }
        // v2 and v3 explicitly name the item type, with 16- or 32-bit IDs.
        let Some([v]) = info.prefix::<1>(r) else {
            continue;
        };
        let id_width = match v {
            2 => 2,
            3 => 4,
            _ => continue,
        };
        let Some(fields) = info.after(4) else {
            continue;
        };
        if fields.after(id_width).and_then(|span| span.prefix::<6>(r)) == Some(*b"\0\0Exif") {
            let id = if id_width == 2 {
                fields.prefix::<2>(r).map(|raw| u32::from(be16(&raw)))
            } else {
                fields.prefix::<4>(r).map(|raw| be32(&raw))
            };
            ids.extend(id);
        }
    }
    if ids.is_empty() {
        return None;
    }
    ids.sort_unstable();
    let id = if let (Some(primary), Some(iref)) = (primary, find(meta, b"iref")) {
        referenced_exif(r, iref, primary, &ids)?
    } else if let [only] = ids[..] {
        only
    } else {
        return None;
    };
    let loc = find(meta, b"iloc")?.bytes(r, MAX_ITEM_TABLE)?;
    read_extents(r, &loc, id, file, find(meta, b"idat"))
}

fn referenced_exif(
    r: &mut (impl Read + Seek),
    iref: Span,
    primary: u32,
    ids: &[u32],
) -> Option<u32> {
    let width = match iref.prefix::<1>(r)?[0] {
        0 => 2,
        1 => 4,
        _ => return None,
    };
    let mut rest = iref.after(4)?;
    for _ in 0..MAX_BOXES {
        let (kind, span) = next_atom(r, &mut rest)?;
        if &kind != b"cdsc" {
            continue;
        }
        let data = span.bytes(r, (MAX_BOXES + 1) * width + 2)?;
        let mut buf = data.as_slice();
        let from = number(&mut buf, width)? as u32;
        if ids.binary_search(&from).is_err() {
            continue;
        }
        for _ in 0..number(&mut buf, 2)?.min(MAX_BOXES as u64) {
            if number(&mut buf, width)? == u64::from(primary) {
                return Some(from);
            }
        }
    }
    None
}

fn read_extents(
    r: &mut (impl Read + Seek),
    mut buf: &[u8],
    id: u32,
    file: Span,
    idat: Option<Span>,
) -> Option<Vec<u8>> {
    let version = number(&mut buf, 4)? >> 24;
    if version > 2 {
        return None;
    }
    let widths = number(&mut buf, 2)? as usize;
    let offset = widths >> 12;
    let length = (widths >> 8) & 15;
    let base = (widths >> 4) & 15;
    let index = if version == 0 { 0 } else { widths & 15 };
    if [offset, length, base, index]
        .iter()
        .any(|&w| !matches!(w, 0 | 4 | 8))
    {
        return None;
    }
    let id_width = if version == 2 { 4 } else { 2 };
    for _ in 0..number(&mut buf, id_width)?.min(MAX_BOXES as u64) {
        let item = number(&mut buf, id_width)?;
        let method = if version == 0 {
            0
        } else {
            number(&mut buf, 2)? & 15
        };
        let reference = number(&mut buf, 2)?;
        let base_offset = number(&mut buf, base)?;
        let count = number(&mut buf, 2)? as usize;
        if item != u64::from(id) {
            buf = buf.get(count * (index + offset + length)..)?;
            continue;
        }
        if reference != 0 || count > MAX_BOXES {
            return None;
        }
        let source = match method {
            0 => file,
            1 => idat?,
            _ => return None,
        };
        let source = source.after(base_offset)?;
        let mut out = Vec::new();
        for _ in 0..count {
            number(&mut buf, index)?;
            let extent = source.after(number(&mut buf, offset)?)?;
            let n = number(&mut buf, length)?;
            let n = if n == 0 { extent.end - extent.start } else { n };
            extent.after(n)?;
            let n = usize::try_from(n).ok()?;
            if n > MAX_EXIF - out.len() {
                return None;
            }
            let old = out.len();
            out.resize(old + n, 0);
            r.seek(SeekFrom::Start(extent.start)).ok()?;
            r.read_exact(&mut out[old..]).ok()?;
        }
        return Some(out);
    }
    None
}
