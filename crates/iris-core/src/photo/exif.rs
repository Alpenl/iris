//! Unified TIFF-formatted EXIF parser (orientation + camera/exposure summary).

use super::icc::{be16, be32};

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

#[derive(Default)]
pub(super) struct ParsedExif {
    pub orientation: Option<u16>,
    pub summary: Option<ExifSummary>,
}

/// Parses IFD0 (and optionally `ExifIFD`) from a TIFF-formatted EXIF payload.
pub(super) fn parse_exif_tiff(buf: &[u8], include_summary: bool) -> ParsedExif {
    let mut out = ParsedExif::default();
    if buf.len() < 8 {
        return out;
    }
    let le = match &buf[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return out,
    };
    let u16_ = |off: usize| -> Option<u16> {
        let s = buf.get(off..off + 2)?;
        Some(if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            be16(s)
        })
    };
    let u32_ = |off: usize| -> Option<u32> {
        let s = buf.get(off..off + 4)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            be32(s)
        })
    };
    let read_ascii = |entry_off: usize, count: usize| -> Option<String> {
        if count == 0 || count > 256 {
            return None;
        }
        let start = if count <= 4 {
            entry_off + 8
        } else {
            u32_(entry_off + 8)? as usize
        };
        let bytes = buf.get(start..start.checked_add(count)?)?;
        let trimmed = bytes.split(|&b| b == 0).next().unwrap_or(bytes);
        let text = String::from_utf8_lossy(trimmed).trim().to_string();
        (!text.is_empty()).then_some(text)
    };
    let read_rational = |entry_off: usize| -> Option<(u32, u32)> {
        let off = u32_(entry_off + 8)? as usize;
        let num = u32_(off)?;
        let den = u32_(off.checked_add(4)?)?;
        (den != 0).then_some((num, den))
    };

    let Some(ifd0) = u32_(4).map(|v| v as usize) else {
        return out;
    };
    let Some(count0) = u16_(ifd0).map(|c| c.min(128) as usize) else {
        return out;
    };

    let mut summary = ExifSummary::default();
    let mut make: Option<String> = None;
    let mut model: Option<String> = None;
    let mut exif_ifd_off: Option<usize> = None;

    for i in 0..count0 {
        let Some(off) = ifd0.checked_add(2).and_then(|v| v.checked_add(i * 12)) else {
            break;
        };
        let (Some(tag), Some(ty), Some(cnt)) = (u16_(off), u16_(off + 2), u32_(off + 4)) else {
            break;
        };
        if tag == 0x0112
            && let Some(val) = u16_(off + 8).filter(|v| (1..=8).contains(v))
        {
            out.orientation = Some(val);
            if !include_summary {
                return out;
            }
        }
        if !include_summary {
            continue;
        }
        match (tag, ty) {
            (0x010F, 2) => make = read_ascii(off, cnt as usize),
            (0x0110, 2) => model = read_ascii(off, cnt as usize),
            (0x0132, 2) if summary.date_taken.is_none() => {
                summary.date_taken = read_ascii(off, cnt as usize);
            }
            (0x8769, 4) => exif_ifd_off = u32_(off + 8).map(|v| v as usize),
            _ => {}
        }
    }

    if !include_summary {
        return out;
    }

    summary.camera = match (make, model) {
        (Some(mk), Some(md)) => {
            if md
                .to_ascii_lowercase()
                .starts_with(&mk.to_ascii_lowercase())
            {
                Some(md)
            } else {
                Some(format!("{mk} {md}"))
            }
        }
        (None, Some(md)) => Some(md),
        (Some(mk), None) => Some(mk),
        (None, None) => None,
    };

    if let Some(exif_off) = exif_ifd_off
        && let Some(count_e) = u16_(exif_off)
    {
        for i in 0..count_e.min(128) as usize {
            let Some(off) = exif_off.checked_add(2).and_then(|v| v.checked_add(i * 12)) else {
                break;
            };
            let (Some(tag), Some(ty), Some(cnt)) = (u16_(off), u16_(off + 2), u32_(off + 4)) else {
                break;
            };
            match (tag, ty) {
                (0x829A, 5) => {
                    if let Some((num, den)) = read_rational(off) {
                        summary.exposure = Some(if num >= den {
                            format!("{:.1}s", num as f64 / den as f64)
                        } else if num > 0 {
                            format!("1/{:.0}s", (den as f64 / num as f64).round())
                        } else {
                            "0s".into()
                        });
                    }
                }
                (0x829D, 5) => {
                    if let Some((num, den)) = read_rational(off) {
                        summary.aperture = Some(format!("f/{:.1}", num as f64 / den as f64));
                    }
                }
                (0x8827, 3) => summary.iso = u16_(off + 8).map(u32::from),
                (0x9003, 2) => summary.date_taken = read_ascii(off, cnt as usize),
                (0x920A, 5) => {
                    if let Some((num, den)) = read_rational(off) {
                        let mm = num as f64 / den as f64;
                        summary.focal_length = Some(if (mm - mm.round()).abs() < 0.05 {
                            format!("{:.0}mm", mm.round())
                        } else {
                            format!("{mm:.1}mm")
                        });
                    }
                }
                (0xA434, 2) => summary.lens = read_ascii(off, cnt as usize),
                _ => {}
            }
        }
    }

    if !summary.is_empty() {
        out.summary = Some(summary);
    }
    out
}
