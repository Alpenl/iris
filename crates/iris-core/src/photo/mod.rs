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
    /// HEIF only. Applied as: mirror in source space, then rotate clockwise.
    pub mirror: Option<Mirror>,
    pub rotate_cw: u16,
    /// EXIF value before decoder-owned orientation is applied.
    pub exif_orientation: Option<u16>,
    pub(crate) heif: bool,
    pub(crate) filter_orientation: bool,
    pub exif: Option<ExifSummary>,
}

impl Photo {
    pub fn is_heif(&self) -> bool {
        self.heif
    }

    /// Returns whether `vf` (`hflip`/`vflip` + `format=rotate=...`) owns orientation
    /// instead of relying on `libavcodec`'s `DISPLAYMATRIX`.
    pub(crate) fn uses_filter_orientation(&self) -> bool {
        self.filter_orientation
            || (self.heif && (self.size.is_some() || self.rotate_cw != 0 || self.mirror.is_some()))
    }

    /// Clockwise orientation of the displayed photo, before user rotation.
    pub fn display_rotation(&self) -> u16 {
        if self.uses_filter_orientation() || self.is_heif() {
            self.rotate_cw
        } else {
            match self.exif_orientation {
                Some(3 | 4) => 180,
                Some(5 | 6) => 90,
                Some(7 | 8) => 270,
                _ => 0,
            }
        }
    }

    /// Video filter chain with an optional extra clockwise rotation (`0/90/180/270`).
    pub(crate) fn vf_chain(&self, extra_rotate_cw: u16) -> Option<String> {
        self.vf_chain_with_transforms(
            extra_rotate_cw,
            false,
            false,
            self.uses_filter_orientation(),
        )
    }

    /// Combines user flips expressed in source space with a HEIF source mirror.
    /// Keep the format filter last so the mirror filters cannot strip colour metadata.
    pub(crate) fn vf_chain_with_transforms(
        &self,
        extra_rotate_cw: u16,
        extra_hflip: bool,
        extra_vflip: bool,
        set_filter_rotation: bool,
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
        let net_hflip = (self.mirror == Some(Mirror::Horizontal)) ^ extra_hflip;
        let net_vflip = (self.mirror == Some(Mirror::Vertical)) ^ extra_vflip;
        if net_hflip {
            vf.push("hflip".into());
        }
        if net_vflip {
            vf.push("vflip".into());
        }
        let colour = self.colour.and_then(|c| c.mpv_format());
        let mut format: Vec<String> = colour.into_iter().collect();
        let rot = (self.rotate_cw + extra_rotate_cw % 360) % 360;
        if set_filter_rotation && (rot != 0 || self.rotate_cw != 0 || self.mirror.is_some()) {
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

fn exif_to_source_transform(exif_orientation: u16) -> (Option<Mirror>, u16) {
    match exif_orientation {
        2 => (Some(Mirror::Horizontal), 0),
        3 => (None, 180),
        4 => (Some(Mirror::Vertical), 0),
        5 => (Some(Mirror::Vertical), 90),
        6 => (None, 90),
        7 => (Some(Mirror::Vertical), 270),
        8 => (None, 270),
        _ => (None, 0),
    }
}

fn source_transform_to_exif(mirror: Option<Mirror>, rotate_cw: u16) -> u16 {
    match (mirror, rotate_cw % 360) {
        (None, 90) => 6,
        (None, 180) => 3,
        (None, 270) => 8,
        (Some(Mirror::Horizontal), 0) | (Some(Mirror::Vertical), 180) => 2,
        (Some(Mirror::Vertical), 0) | (Some(Mirror::Horizontal), 180) => 4,
        (Some(Mirror::Vertical), 90) | (Some(Mirror::Horizontal), 270) => 5,
        (Some(Mirror::Vertical), 270) | (Some(Mirror::Horizontal), 90) => 7,
        _ => 1,
    }
}

fn finish_header(mut h: Header, fmt: PhotoFormat) -> Photo {
    let colour = match h.colour {
        Some(ColourTag::Icc(raw)) => icc::classify_icc(&raw),
        Some(ColourTag::Nclx {
            primaries,
            transfer,
        }) => icc::classify_nclx(primaries, transfer),
        None => None,
    };
    let heif = fmt == PhotoFormat::Heif;
    let mut filter_orientation =
        heif && (h.size.is_some() || h.rotate_cw != 0 || h.mirror.is_some());
    match (fmt, h.exif_orientation) {
        (PhotoFormat::Webp | PhotoFormat::Tiff | PhotoFormat::Png, Some(orient @ 2..=8))
        | (PhotoFormat::Jpeg, Some(orient @ (2 | 4 | 5 | 7))) => {
            let (mirror, rotate_cw) = exif_to_source_transform(orient);
            h.mirror = mirror;
            h.rotate_cw = rotate_cw;
            filter_orientation = true;
        }
        _ => {}
    }
    Photo {
        colour,
        size: h.size,
        mirror: h.mirror,
        rotate_cw: h.rotate_cw,
        exif_orientation: h.exif_orientation,
        heif,
        filter_orientation,
        exif: h.exif.filter(|e| !e.is_empty()),
    }
}

fn run_probe<R: Read + Seek>(r: &mut R, fmt: PhotoFormat) -> Header {
    let mut h = Header::default();
    match fmt {
        PhotoFormat::Jpeg => containers::jpeg(r, &mut h, false),
        PhotoFormat::Png => containers::png(r, &mut h, false),
        PhotoFormat::Webp => containers::webp(r, &mut h, false),
        PhotoFormat::Heif => heif::probe(r, &mut h),
        PhotoFormat::Tiff => exif::tiff(r, &mut h, exif::Mode::Image),
    };
    h
}

/// Reads orientation (`1..=8`, matching EXIF orientation semantics) from a JPEG, TIFF,
/// WebP, PNG, or HEIF (`irot`/`imir`) file header using magic-byte sniffing first and
/// extension fallback. Returns `1` on unknown/absent.
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
        PhotoFormat::Png => {
            let _ = containers::png(&mut r, &mut h, true);
        }
        PhotoFormat::Webp => {
            let _ = containers::webp(&mut r, &mut h, true);
        }
        PhotoFormat::Tiff => {
            let _ = exif::tiff(&mut r, &mut h, exif::Mode::Orientation);
        }
        PhotoFormat::Heif => {
            let _ = heif::probe(&mut r, &mut h);
            return source_transform_to_exif(h.mirror, h.rotate_cw);
        }
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
    finish_header(run_probe(&mut r, fmt), fmt)
}

/// Probes a photo header from in-memory bytes (e.g. a Range fetch of a remote photo),
/// using magic-byte sniffing first and an optional filename/extension hint as fallback.
pub fn probe_bytes(bytes: &[u8], name_or_ext_hint: Option<&str>) -> Photo {
    let Some(fmt) = detect_photo_format(bytes, name_or_ext_hint) else {
        return Photo::default();
    };
    finish_header(run_probe(&mut Cursor::new(bytes), fmt), fmt)
}

#[cfg(test)]
mod tests {
    use super::{
        Colour, Header, Mirror, Photo, containers, exif_orientation, probe, probe_bytes,
        source_transform_to_exif,
    };
    use crate::test_util::TestDir;
    use std::path::{Path, PathBuf};

    fn media_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/photo")
    }

    fn media(rel: &str) -> Option<PathBuf> {
        let p = media_root().join(rel);
        p.exists().then_some(p)
    }

    #[test]
    fn reads_colour_size_and_heif_rotation_from_test_media() {
        let icc_dir = media_root().join("icc");
        if icc_dir.is_dir() {
            for (file, want) in [
                ("icc/red-untagged.png", None),
                ("icc/red-srgb.png", Some("bt.709")),
                ("icc/red-p3.png", Some("display-p3")),
                ("icc/red-adobergb.png", Some("adobe")),
                ("icc/red-p3-jpeg.jpg", Some("display-p3")),
                ("icc/red-p3-webp.webp", Some("display-p3")),
            ] {
                let p = media_root().join(file);
                assert!(p.exists(), "missing canonical ICC fixture {file}");
                assert_eq!(probe(&p).colour.map(|c| c.primaries), want, "{file}");
            }
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
            if file == "exif/heic-o6.heic" {
                assert_eq!(exif_orientation(&p), 6);
            }
        }
    }

    #[test]
    fn heif_irot_and_imir_map_to_exif_orientation() {
        assert_eq!(source_transform_to_exif(None, 0), 1);
        assert_eq!(source_transform_to_exif(Some(Mirror::Horizontal), 0), 2);
        assert_eq!(source_transform_to_exif(None, 180), 3);
        assert_eq!(source_transform_to_exif(Some(Mirror::Vertical), 0), 4);
        assert_eq!(source_transform_to_exif(Some(Mirror::Vertical), 90), 5);
        assert_eq!(source_transform_to_exif(None, 90), 6);
        assert_eq!(source_transform_to_exif(Some(Mirror::Vertical), 270), 7);
        assert_eq!(source_transform_to_exif(None, 270), 8);

        // Synthetic minimal HEIF with `ftyp(heic)` + `meta` -> `iprp` -> `ipco` -> `irot(3)` (90° CW).
        fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }
        let ipco = atom(b"ipco", &atom(b"irot", &[3]));
        let iprp = atom(b"iprp", &ipco);
        let mut meta_body = vec![0, 0, 0, 0];
        meta_body.extend_from_slice(&iprp);
        let mut heif = atom(b"ftyp", b"heic\0\0\0\0heic");
        heif.extend_from_slice(&atom(b"meta", &meta_body));

        let dir = TestDir::new("photo-heif-irot");
        let file = dir.join("rotated.heic");
        std::fs::write(&file, &heif).unwrap();
        assert_eq!(exif_orientation(&file), 6);
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
            heif: true,
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

        // A source-space vertical flip preserves the P3 format tail.
        assert_eq!(
            portrait
                .vf_chain_with_transforms(0, false, true, true)
                .as_deref(),
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
            p3_png
                .vf_chain_with_transforms(0, true, false, false)
                .as_deref(),
            Some("hflip,format=primaries=display-p3:gamma=srgb")
        );
    }

    #[test]
    fn probe_and_exif_orientation_sniff_magic_bytes_even_with_missing_or_wrong_extension() {
        let dir = TestDir::new("photo-sniff");

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
        assert_eq!(probe(&disguised).display_rotation(), 90);
        assert_eq!(probe(&disguised).size, Some((4000, 3000)));
        assert_eq!(probe_bytes(&jpeg_bytes, None).size, Some((4000, 3000)));

        let tiff_file = dir.join("raw_tiff");
        std::fs::write(&tiff_file, &tiff_exif).unwrap();
        assert_eq!(exif_orientation(&tiff_file), 6);
        assert_eq!(
            probe(&tiff_file).vf_chain(0).as_deref(),
            Some("format=rotate=90")
        );

        // PNG with `eXIf` orientation=6 and JPEG with mirrored EXIF=2 / EXIF=5.
        let mut png_bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        png_bytes.extend_from_slice(&13u32.to_be_bytes());
        png_bytes.extend_from_slice(b"IHDR");
        png_bytes.extend_from_slice(&200u32.to_be_bytes());
        png_bytes.extend_from_slice(&100u32.to_be_bytes());
        png_bytes.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        png_bytes.extend_from_slice(&(tiff_exif.len() as u32).to_be_bytes());
        png_bytes.extend_from_slice(b"eXIf");
        png_bytes.extend_from_slice(&tiff_exif);
        png_bytes.extend_from_slice(&[0, 0, 0, 0]);
        png_bytes.extend_from_slice(&0u32.to_be_bytes());
        png_bytes.extend_from_slice(b"IEND");
        png_bytes.extend_from_slice(&[0, 0, 0, 0]);

        let png_file = dir.join("oriented.png");
        std::fs::write(&png_file, &png_bytes).unwrap();
        assert_eq!(exif_orientation(&png_file), 6);
        let probed_png = probe(&png_file);
        assert_eq!(probed_png.display_rotation(), 90);
        assert_eq!(probed_png.vf_chain(0).as_deref(), Some("format=rotate=90"));

        // Mirrored JPEG EXIF 2 must override mjpegdec's 180-deg displaymatrix with hflip + rotate=0.
        let mut tiff_o2 = tiff_exif.clone();
        tiff_o2[18..20].copy_from_slice(&2u16.to_le_bytes());
        let mut app1_o2 = b"Exif\0\0".to_vec();
        app1_o2.extend_from_slice(&tiff_o2);
        let mut jpeg_o2 = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg_o2.extend_from_slice(&((app1_o2.len() + 2) as u16).to_be_bytes());
        jpeg_o2.extend_from_slice(&app1_o2);
        jpeg_o2.extend_from_slice(&[0xFF, 0xDA]);
        let probed_o2 = probe_bytes(&jpeg_o2, Some("jpg"));
        assert_eq!(probed_o2.display_rotation(), 0);
        assert_eq!(
            probed_o2.vf_chain(0).as_deref(),
            Some("hflip,format=rotate=0")
        );
    }
}
