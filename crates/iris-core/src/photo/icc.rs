//! Matches a photo's colour profile (`ICC` or ISO `nclx`) to a colour space mpv knows.

/// What the profile says, in mpv's names (`--vf=format=primaries=help`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub primaries: &'static str,
    /// `None` when the tone curve is not one mpv names; mpv then guesses.
    pub gamma: Option<&'static str>,
}

impl Colour {
    /// Human-readable colour space label for media info chips.
    pub fn display_name(&self) -> &'static str {
        match (self.primaries, self.gamma) {
            ("bt.709", Some("pq")) => "BT.709 PQ",
            ("bt.709", Some("hlg")) => "BT.709 HLG",
            ("bt.709", Some("srgb")) => "sRGB",
            ("bt.709", _) => "BT.709",
            ("display-p3", _) => "Display P3",
            ("adobe", _) => "Adobe RGB",
            ("bt.2020", _) => "BT.2020",
            ("prophoto", _) => "ProPhoto",
            (other, _) => other,
        }
    }

    /// Parameters for mpv's `format` video filter, or `None` when mpv's
    /// default (BT.709 primaries, sRGB curve) is already right.
    pub(crate) fn mpv_format(&self) -> Option<String> {
        if self.primaries == "bt.709" && matches!(self.gamma, None | Some("srgb")) {
            return None;
        }
        let mut params = format!("primaries={}", self.primaries);
        if let Some(g) = self.gamma {
            params.push_str(":gamma=");
            params.push_str(g);
        }
        Some(params)
    }
}

pub(super) fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

pub(super) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Chromaticities (x, y) of the r/g/b colorants Bradford-adapted to D50.
const KNOWN_PRIMARIES: &[(&str, [[f64; 2]; 3])] = &[
    (
        "bt.709",
        [[0.6484, 0.3309], [0.3212, 0.5979], [0.1559, 0.0661]],
    ),
    (
        "display-p3",
        [[0.6820, 0.3193], [0.2846, 0.6746], [0.1559, 0.0661]],
    ),
    (
        "adobe",
        [[0.6484, 0.3309], [0.2302, 0.7016], [0.1559, 0.0661]],
    ),
    (
        "bt.2020",
        [[0.7085, 0.2935], [0.1902, 0.7754], [0.1292, 0.0471]],
    ),
    (
        "prophoto",
        [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]],
    ),
];

const XY_TOLERANCE: f64 = 0.004;

/// Matches an ICC profile; `None` for profiles that are not RGB matrix/TRC
/// profiles of a known colour space.
pub fn classify_icc(icc: &[u8]) -> Option<Colour> {
    if icc.len() < 132 || &icc[36..40] != b"acsp" || &icc[16..20] != b"RGB " {
        return None;
    }
    let count = (be32(&icc[128..]) as usize).min(1024);
    let tag = |sig: &[u8; 4]| -> Option<&[u8]> {
        (0..count).find_map(|i| {
            let e = icc.get(132 + 12 * i..144 + 12 * i)?;
            if &e[..4] != sig {
                return None;
            }
            let (off, len) = (be32(&e[4..]) as usize, be32(&e[8..]) as usize);
            icc.get(off..off.checked_add(len)?)
        })
    };
    let xy = |sig: &[u8; 4]| -> Option<[f64; 2]> {
        let t = tag(sig)?;
        if t.len() < 20 || &t[..4] != b"XYZ " {
            return None;
        }
        let v = [
            be32(&t[8..]) as i32 as f64 / 65536.0,
            be32(&t[12..]) as i32 as f64 / 65536.0,
            be32(&t[16..]) as i32 as f64 / 65536.0,
        ];
        let sum = v[0] + v[1] + v[2];
        (sum > 0.0).then(|| [v[0] / sum, v[1] / sum])
    };
    let measured = [xy(b"rXYZ")?, xy(b"gXYZ")?, xy(b"bXYZ")?];
    let primaries = KNOWN_PRIMARIES.iter().find_map(|(name, want)| {
        let close = measured.iter().zip(want).all(|(m, w)| {
            (m[0] - w[0]).abs() <= XY_TOLERANCE && (m[1] - w[1]).abs() <= XY_TOLERANCE
        });
        close.then_some(*name)
    })?;
    let curves = [tag(b"rTRC")?, tag(b"gTRC")?, tag(b"bTRC")?];
    let gamma = if curves[1..].iter().all(|c| *c == curves[0]) {
        classify_trc(curves[0])
    } else {
        None
    };
    Some(Colour { primaries, gamma })
}

fn classify_trc(t: &[u8]) -> Option<&'static str> {
    if t.len() < 12 {
        return None;
    }
    let s15 = |o: usize| t.get(o..o + 4).map(|b| be32(b) as i32 as f64 / 65536.0);
    match &t[..4] {
        b"curv" => {
            let n = be32(&t[8..]) as usize;
            match n {
                0 => Some("linear"),
                1 => pure_gamma(be16(t.get(12..14)?) as f64 / 256.0),
                _ => {
                    let end = n.checked_mul(2)?.checked_add(12)?;
                    let table = t.get(12..end)?;
                    let fits = (0..n).step_by((n / 32).max(1)).all(|i| {
                        let x = i as f64 / (n - 1) as f64;
                        let y = be16(&table[2 * i..]) as f64 / 65535.0;
                        (y - srgb_eotf(x)).abs() < 0.004
                    });
                    fits.then_some("srgb")
                }
            }
        }
        b"para" => {
            let g = s15(12)?;
            match be16(&t[8..]) {
                0 => pure_gamma(g),
                3 | 4 => {
                    let (a, b, c, d) = (s15(16)?, s15(20)?, s15(24)?, s15(28)?);
                    let srgb = (g - 2.4).abs() < 0.01
                        && (a - 1.0 / 1.055).abs() < 0.002
                        && (b - 0.055 / 1.055).abs() < 0.002
                        && (c - 1.0 / 12.92).abs() < 0.002
                        && (d - 0.04045).abs() < 0.002;
                    let romm = (g - 1.8).abs() < 0.01 && (a - 1.0).abs() < 0.002 && b.abs() < 0.002;
                    if srgb {
                        Some("srgb")
                    } else if romm {
                        Some("prophoto")
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn pure_gamma(g: f64) -> Option<&'static str> {
    [
        (1.0, "linear"),
        (1.8, "gamma1.8"),
        (2.0, "gamma2.0"),
        (2.2, "gamma2.2"),
        (2.4, "gamma2.4"),
        (2.6, "gamma2.6"),
        (2.8, "gamma2.8"),
    ]
    .iter()
    .find(|(v, _)| (g - v).abs() < 0.02)
    .map(|(_, name)| *name)
}

fn srgb_eotf(x: f64) -> f64 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// Matches ISO/IEC 23091-2 code points from a HEIF `nclx` colour box.
pub fn classify_nclx(primaries: u16, transfer: u16) -> Option<Colour> {
    let primaries = match primaries {
        1 => "bt.709",
        9 => "bt.2020",
        11 => "dci-p3",
        12 => "display-p3",
        _ => return None,
    };
    let gamma = match transfer {
        8 => Some("linear"),
        13 => Some("srgb"),
        16 => Some("pq"),
        18 => Some("hlg"),
        _ => None,
    };
    Some(Colour { primaries, gamma })
}

#[cfg(test)]
mod tests {
    use super::{Colour, classify_icc, classify_nclx};
    use std::path::Path;

    const PROFILES: &str = "/System/Library/ColorSync/Profiles";

    fn system(name: &str) -> Option<Vec<u8>> {
        std::fs::read(Path::new(PROFILES).join(name)).ok()
    }

    #[test]
    fn matches_system_profiles() {
        let cases = [
            ("sRGB Profile.icc", "bt.709", Some("srgb")),
            ("Display P3.icc", "display-p3", Some("srgb")),
            ("AdobeRGB1998.icc", "adobe", Some("gamma2.2")),
            ("ROMM RGB.icc", "prophoto", Some("prophoto")),
            ("ITU-2020.icc", "bt.2020", None),
        ];
        for (file, primaries, gamma) in cases {
            let Some(icc) = system(file) else { continue };
            assert_eq!(
                classify_icc(&icc),
                Some(Colour { primaries, gamma }),
                "{file}"
            );
        }
        if let Some(icc) = system("Generic RGB Profile.icc") {
            assert_eq!(classify_icc(&icc), None);
        }
    }

    #[test]
    fn bt709_hdr_transfer_is_not_labelled_srgb() {
        assert_eq!(classify_nclx(1, 16).unwrap().display_name(), "BT.709 PQ");
        assert_eq!(classify_nclx(1, 18).unwrap().display_name(), "BT.709 HLG");
        assert_eq!(classify_nclx(1, 13).unwrap().display_name(), "sRGB");
    }

    #[test]
    fn mpv_format_skips_default_and_malformed_icc_never_panics() {
        let srgb = Colour {
            primaries: "bt.709",
            gamma: Some("srgb"),
        };
        assert_eq!(srgb.mpv_format(), None);
        let p3 = Colour {
            primaries: "display-p3",
            gamma: Some("srgb"),
        };
        assert_eq!(
            p3.mpv_format().as_deref(),
            Some("primaries=display-p3:gamma=srgb")
        );

        assert_eq!(classify_icc(&[]), None);
        assert_eq!(classify_icc(&[0u8; 200]), None);
        let mut fake = vec![0u8; 160];
        fake[16..20].copy_from_slice(b"RGB ");
        fake[36..40].copy_from_slice(b"acsp");
        fake[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(classify_icc(&fake), None);
    }
}
