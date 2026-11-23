//! Photo header probing (colour profiles, dimensions, HEIF transforms, EXIF)
//! and structured per-file `mpv` filter generation.

mod containers;
mod exif;
mod heif;
pub mod icc;
mod sparse;

use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::path::Path;

pub use exif::ExifSummary;
pub use icc::Colour;
pub use sparse::probe_remote;

use containers::{ColourTag, Header, PhotoFormat, detect_photo_format};

/// Longest side handed to mpv's GPU renderer (leaves 48 MP 8064×6048 untouched).
const MAX_SIDE: u32 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    /// Left-right (`hflip`).
    Horizontal,
    /// Top-bottom (`vflip`).
    Vertical,
}

/// What a photo's header says.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Photo {
    pub colour: Option<Colour>,
    /// Stored pixel size, before any rotation.
    pub size: Option<(u32, u32)>,
    /// HEIF only (mpv handles JPEG EXIF itself). Applied as: mirror first, then rotate CW.
    pub mirror: Option<Mirror>,
    pub rotate_cw: u16,
    pub exif: Option<ExifSummary>,
}

impl Photo {
    /// Video filter chain with an optional extra clockwise rotation (`0/90/180/270`).
    pub(crate) fn vf_chain(&self, extra_rotate_cw: u16) -> Option<String> {
        self.vf_chain_with_transforms(extra_rotate_cw, false, false)
    }

    /// Video filter chain combining optional user `hflip`/`vflip` (in upright image space)
    /// and extra clockwise rotation while keeping `format=primaries=...:gamma=...:rotate=...`
    /// at the tail so `lavfi` mirror filters never strip wide-gamut or rotation metadata.
    pub(crate) fn vf_chain_with_transforms(
        &self,
        extra_rotate_cw: u16,
        extra_hflip: bool,
        extra_vflip: bool,
    ) -> Option<String> {
        let mut vf = Vec::new();
        if let Some((w, h)) = self.size
            && w.max(h) > MAX_SIDE
        {
            vf.push(if w >= h {
                format!("scale=w={MAX_SIDE}:h=-2")
            } else {
                format!("scale=w=-2:h={MAX_SIDE}")
            });
        }
        let (pre_h, pre_v) = if self.rotate_cw.is_multiple_of(180) {
            (extra_hflip, extra_vflip)
        } else {
            (extra_vflip, extra_hflip)
        };
        let net_hflip = (self.mirror == Some(Mirror::Horizontal)) ^ pre_h;
        let net_vflip = (self.mirror == Some(Mirror::Vertical)) ^ pre_v;
        if net_hflip {
            vf.push("hflip".into());
        }
        if net_vflip {
            vf.push("vflip".into());
        }
        let colour = self.colour.and_then(|c| c.mpv_format());
        let mut format: Vec<String> = colour.into_iter().collect();
        let rot = (self.rotate_cw + extra_rotate_cw) % 360;
        if rot != 0 {
            format.push(format!("rotate={rot}"));
        }
        if !format.is_empty() {
            vf.push(format!("format={}", format.join(":")));
        }
        (!vf.is_empty()).then(|| vf.join(","))
    }

    /// Structured per-file `loadfile_with_options` key-value pairs.
    pub(crate) fn mpv_options(&self) -> Vec<(&'static str, String)> {
        let mut opts = Vec::new();
        if let Some(chain) = self.vf_chain(0) {
            opts.push(("vf", chain));
        }
        if self.colour.and_then(|c| c.mpv_format()).is_some() {
            opts.push(("gamut-mapping-mode", "clip".into()));
        }
        opts
    }
}

fn finish_header(h: Header) -> Photo {
    let colour = match h.colour {
        Some(ColourTag::Icc(raw)) => icc::classify_icc(&raw),
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
        exif: h.exif.filter(|e| !e.is_empty()),
    }
}

fn run_probe<R: Read + Seek>(r: &mut R, fmt: PhotoFormat) -> Header {
    let mut h = Header::default();
    match fmt {
        PhotoFormat::Jpeg => containers::jpeg(r, &mut h, false),
        PhotoFormat::Png => containers::png(r, &mut h),
        PhotoFormat::Webp => containers::webp(r, &mut h),
        PhotoFormat::Heif => heif::probe(r, &mut h),
        PhotoFormat::Tiff => exif::tiff(r, &mut h, exif::Mode::Image),
    };
    h
}

/// Reads EXIF orientation (`1..=8`) from a JPEG or TIFF file header using
/// magic-byte sniffing first and extension fallback. Returns `1` on unknown/absent.
pub fn exif_orientation(path: &Path) -> u16 {
    let Ok(file) = File::open(path) else {
        return 1;
    };
    let mut r = BufReader::new(file);
    let prefix = r.fill_buf().unwrap_or(&[]);
    let hint = path.file_name().and_then(|n| n.to_str());
    let Some(fmt) = detect_photo_format(prefix, hint) else {
        return 1;
    };
    let mut h = Header::default();
    match fmt {
        PhotoFormat::Jpeg => {
            let _ = containers::jpeg(&mut r, &mut h, true);
        }
        PhotoFormat::Tiff => {
            let _ = exif::tiff(&mut r, &mut h, exif::Mode::Orientation);
        }
        _ => return 1,
    }
    h.exif_orientation.unwrap_or(1)
}

/// Probes a local photo's header using magic-byte sniffing first and extension fallback.
pub fn probe(path: &Path) -> Photo {
    let Ok(file) = File::open(path) else {
        return Photo::default();
    };
    let mut r = BufReader::new(file);
    let prefix = r.fill_buf().unwrap_or(&[]);
    let hint = path.file_name().and_then(|n| n.to_str());
    let Some(fmt) = detect_photo_format(prefix, hint) else {
        return Photo::default();
    };
    finish_header(run_probe(&mut r, fmt))
}

/// Probes a photo header from in-memory bytes (e.g. a Range fetch of a remote photo),
/// using magic-byte sniffing first and an optional filename/extension hint as fallback.
pub fn probe_bytes(bytes: &[u8], name_or_ext_hint: Option<&str>) -> Photo {
    let Some(fmt) = detect_photo_format(bytes, name_or_ext_hint) else {
        return Photo::default();
    };
    finish_header(run_probe(&mut Cursor::new(bytes), fmt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn media(rel: &str) -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-media/photo")
            .join(rel);
        p.exists().then_some(p)
    }

    #[test]
    fn reads_colour_size_and_heif_rotation_from_test_media() {
        for (file, want) in [
            ("icc/red-untagged.png", None),
            ("icc/red-srgb.png", Some("bt.709")),
            ("icc/red-p3.png", Some("display-p3")),
            ("icc/red-adobergb.png", Some("adobe")),
            ("icc/red-p3-jpeg.jpg", Some("display-p3")),
            ("icc/red-p3-webp.webp", Some("display-p3")),
        ] {
            let Some(p) = media(file) else { continue };
            assert_eq!(probe(&p).colour.map(|c| c.primaries), want, "{file}");
        }
        for (file, size, rotate) in [
            ("exif/exif-o6.jpg", (3000, 2000), 0),
            ("exif/heic-o6.heic", (3000, 2000), 90),
            ("heic/heic-48mp-8064x6048.heic", (8064, 6048), 0),
            ("huge/huge-20000x3000.png", (20000, 3000), 0),
        ] {
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
    fn structured_mpv_options_and_mirror_math() {
        assert!(Photo::default().mpv_options().is_empty());
        let portrait = Photo {
            colour: Some(Colour {
                primaries: "display-p3",
                gamma: Some("srgb"),
            }),
            size: Some((4000, 40000)),
            rotate_cw: 90,
            ..Default::default()
        };
        assert_eq!(
            portrait.mpv_options(),
            vec![
                (
                    "vf",
                    "scale=w=-2:h=8192,format=primaries=display-p3:gamma=srgb:rotate=90".into()
                ),
                ("gamut-mapping-mode", "clip".into()),
            ]
        );

        let mut h = Header {
            rotate_cw: 90,
            ..Default::default()
        };
        containers::add_mirror(&mut h, Mirror::Horizontal);
        assert_eq!((h.mirror, h.rotate_cw), (Some(Mirror::Horizontal), 270));
        containers::add_mirror(&mut h, Mirror::Vertical);
        assert_eq!((h.mirror, h.rotate_cw), (None, 270));

        // User mirror on a wide-gamut portrait HEIF commutes before `rotate=90` and preserves P3 format tail.
        assert_eq!(
            portrait.vf_chain_with_transforms(0, true, false).as_deref(),
            Some("scale=w=-2:h=8192,vflip,format=primaries=display-p3:gamma=srgb:rotate=90")
        );
        let p3_png = Photo {
            colour: Some(Colour {
                primaries: "display-p3",
                gamma: Some("srgb"),
            }),
            size: Some((1200, 900)),
            ..Default::default()
        };
        assert_eq!(
            p3_png.vf_chain_with_transforms(0, true, false).as_deref(),
            Some("hflip,format=primaries=display-p3:gamma=srgb")
        );
    }

    #[test]
    fn probe_and_exif_orientation_sniff_magic_bytes_even_with_missing_or_wrong_extension() {
        let dir = std::env::temp_dir().join(format!("iris-photo-sniff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // JPEG with EXIF orientation=6 saved with `.bin` extension.
        let mut tiff_exif = Vec::new();
        tiff_exif.extend_from_slice(b"II*\0");
        tiff_exif.extend_from_slice(&8u32.to_le_bytes());
        tiff_exif.extend_from_slice(&1u16.to_le_bytes());
        tiff_exif.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff_exif.extend_from_slice(&3u16.to_le_bytes());
        tiff_exif.extend_from_slice(&1u32.to_le_bytes());
        tiff_exif.extend_from_slice(&6u16.to_le_bytes());
        tiff_exif.extend_from_slice(&0u16.to_le_bytes());
        tiff_exif.extend_from_slice(&0u32.to_le_bytes());

        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff_exif);
        let mut jpeg_bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg_bytes.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg_bytes.extend_from_slice(&app1);
        jpeg_bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        jpeg_bytes.extend_from_slice(&3000u16.to_be_bytes());
        jpeg_bytes.extend_from_slice(&4000u16.to_be_bytes());
        jpeg_bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00, 0xFF, 0xDA]);

        let disguised = dir.join("extensionless_jpeg");
        std::fs::write(&disguised, &jpeg_bytes).unwrap();
        assert_eq!(exif_orientation(&disguised), 6);
        assert_eq!(probe(&disguised).size, Some((4000, 3000)));
        assert_eq!(probe_bytes(&jpeg_bytes, None).size, Some((4000, 3000)));

        let tiff_file = dir.join("raw_tiff");
        std::fs::write(&tiff_file, &tiff_exif).unwrap();
        assert_eq!(exif_orientation(&tiff_file), 6);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
