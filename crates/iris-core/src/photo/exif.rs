//! Unified TIFF-formatted EXIF parser (orientation + camera/exposure summary).

use std::io::{Read, Seek, SeekFrom};

use super::containers::{ColourTag, Header, MAX_ICC};

/// Lightweight EXIF summary extracted from file headers (no full decode).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExifSummary {
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub focal_length: Option<String>,
    pub aperture: Option<String>,
    pub exposure: Option<String>,
    pub iso: Option<u32>,
    pub date_taken: Option<String>,
}

impl ExifSummary {
    pub fn is_empty(&self) -> bool {
        self.camera.is_none()
            && self.lens.is_none()
            && self.focal_length.is_none()
            && self.aperture.is_none()
            && self.exposure.is_none()
            && self.iso.is_none()
            && self.date_taken.is_none()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Orientation,
    Summary,
    Image,
}

#[derive(Clone, Copy)]
struct Layout {
    little: bool,
    big: bool,
}

impl Layout {
    fn uint(self, bytes: &[u8]) -> u64 {
        let mut word = [0; 8];
        if self.little {
            word[..bytes.len()].copy_from_slice(bytes);
            u64::from_le_bytes(word)
        } else {
            word[8 - bytes.len()..].copy_from_slice(bytes);
            u64::from_be_bytes(word)
        }
    }

    fn directory<R: Read + Seek>(self, r: &mut R, offset: u64, limit: u64) -> Option<Vec<u8>> {
        r.seek(SeekFrom::Start(offset)).ok()?;
        let mut count = [0; 8];
        let count = &mut count[..if self.big { 8 } else { 2 }];
        r.read_exact(count).ok()?;
        let n = self.uint(count).min(limit) as usize;
        let mut entries = vec![0; n * self.entry_size()];
        // Keep complete entries at the end of a partial prefix as well.
        let mut used = 0;
        while used < entries.len() {
            match r.read(&mut entries[used..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => used += n,
            }
        }
        entries.truncate(used);
        Some(entries)
    }

    fn entry_size(self) -> usize {
        if self.big { 20 } else { 12 }
    }

    fn entry(self, bytes: &[u8]) -> Entry<'_> {
        let value_at = if self.big { 12 } else { 8 };
        Entry {
            layout: self,
            tag: self.uint(&bytes[..2]) as u16,
            ty: self.uint(&bytes[2..4]) as u16,
            count: self.uint(&bytes[4..value_at]),
            field: &bytes[value_at..],
        }
    }
}

#[derive(Clone, Copy)]
struct Entry<'a> {
    layout: Layout,
    tag: u16,
    ty: u16,
    count: u64,
    field: &'a [u8],
}

impl Entry<'_> {
    fn integer(self) -> Option<u64> {
        if self.count != 1 {
            return None;
        }
        let width = match self.ty {
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return None,
        };
        Some(self.layout.uint(self.field.get(..width)?))
    }

    fn read<R: Read + Seek>(self, r: &mut R, out: &mut [u8]) -> Option<()> {
        if let Some(inline) = self.field.get(..out.len()) {
            out.copy_from_slice(inline);
        } else {
            r.seek(SeekFrom::Start(self.layout.uint(self.field))).ok()?;
            r.read_exact(out).ok()?;
        }
        Some(())
    }

    fn ascii<R: Read + Seek>(self, r: &mut R) -> Option<String> {
        let mut scratch = [0; 256];
        let out = scratch.get_mut(..usize::try_from(self.count).ok()?)?;
        self.read(r, out)?;
        let text = String::from_utf8_lossy(out.split(|&b| b == 0).next()?);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn rational<R: Read + Seek>(self, r: &mut R) -> Option<f64> {
        if self.count != 1 {
            return None;
        }
        let mut raw = [0; 8];
        self.read(r, &mut raw)?;
        let den = self.layout.uint(&raw[4..]);
        (den != 0).then(|| self.layout.uint(&raw[..4]) as f64 / den as f64)
    }
}

/// All offsets are relative to the start of this TIFF stream, including embedded EXIF.
pub(super) fn tiff<R: Read + Seek>(r: &mut R, h: &mut Header, mode: Mode) -> Option<()> {
    let mut head = [0; 8];
    r.read_exact(&mut head).ok()?;
    let little = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let mut layout = Layout { little, big: false };
    let ifd = match layout.uint(&head[2..4]) {
        42 => layout.uint(&head[4..]),
        43 if layout.uint(&head[4..6]) == 8 && head[6..] == [0, 0] => {
            layout.big = true;
            r.read_exact(&mut head).ok()?;
            layout.uint(&head)
        }
        _ => return None,
    };
    let entries = layout.directory(r, ifd, 4096)?;
    let mut summary = ExifSummary::default();
    let (mut make, mut model, mut sub_ifd, mut icc) = (None, None, None, None);
    let (mut width, mut height) = (None, None);
    for bytes in entries.chunks_exact(layout.entry_size()) {
        let e = layout.entry(bytes);
        if e.tag == 0x0112
            && let Some(v @ 1..=8) = e.integer()
        {
            h.exif_orientation = Some(v as u16);
            if mode == Mode::Orientation {
                return Some(());
            }
        }
        if mode == Mode::Orientation {
            continue;
        }
        match (e.tag, e.ty) {
            (256, _) => width = e.integer().and_then(|v| u32::try_from(v).ok()),
            (257, _) => height = e.integer().and_then(|v| u32::try_from(v).ok()),
            (34675, 1 | 7) if mode == Mode::Image && (1..=MAX_ICC as u64).contains(&e.count) => {
                icc = Some(e)
            }
            (0x010f, 2) => make = e.ascii(r),
            (0x0110, 2) => model = e.ascii(r),
            (0x0132, 2) => summary.date_taken = e.ascii(r),
            (0x8769, _) => sub_ifd = e.integer(),
            _ => {}
        }
    }
    if mode == Mode::Image {
        h.size = width.zip(height);
    }
    summary.camera = match (make, model) {
        (Some(make), Some(model))
            if !model
                .get(..make.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(&make)) =>
        {
            Some(format!("{make} {model}"))
        }
        (_, Some(model)) => Some(model),
        (make, None) => make,
    };
    if let Some(entries) = sub_ifd.and_then(|off| layout.directory(r, off, 128)) {
        for bytes in entries.chunks_exact(layout.entry_size()) {
            let e = layout.entry(bytes);
            match (e.tag, e.ty) {
                (0x829a, 5) => {
                    summary.exposure = e.rational(r).map(|s| {
                        if s >= 1.0 {
                            format!("{s:.1}s")
                        } else if s > 0.0 {
                            format!("1/{:.0}s", 1.0 / s)
                        } else {
                            "0s".into()
                        }
                    })
                }
                (0x829d, 5) => summary.aperture = e.rational(r).map(|v| format!("f/{v:.1}")),
                (0x8827, 3 | 4) => summary.iso = e.integer().and_then(|v| u32::try_from(v).ok()),
                (0x9003, 2) => {
                    if let Some(date) = e.ascii(r) {
                        summary.date_taken = Some(date);
                    }
                }
                (0x920a, 5) => {
                    summary.focal_length = e.rational(r).map(|v| {
                        if (v - v.round()).abs() < 0.05 {
                            format!("{v:.0}mm")
                        } else {
                            format!("{v:.1}mm")
                        }
                    })
                }
                (0xa434, 2) => summary.lens = e.ascii(r),
                _ => {}
            }
        }
    }
    if !summary.is_empty() {
        h.exif = Some(summary);
    }
    if let Some(e) = icc {
        let mut profile = vec![0; e.count as usize];
        e.read(r, &mut profile)?;
        h.colour = Some(ColourTag::Icc(profile));
    }
    Some(())
}
