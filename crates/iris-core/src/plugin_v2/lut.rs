//! A bounded data-only .cube profile. Only this validated type can construct the
//! fixed preview filter; shader code/filter strings are never accepted from a guest.
use super::{
    files::Artifact,
    ledger::{Ledger, Resource},
};
use crate::vfs::HttpCancel;
use iris_api::v2::ErrorCode;
use std::sync::Arc;
pub struct MonitorLut {
    artifact: Arc<Artifact>,
    pub dimension: u32,
}
impl MonitorLut {
    /// Background-only file validation, bound to an already authorized artifact.
    pub fn prepare(
        artifact: Arc<Artifact>,
        ledger: &Ledger,
        cancel: &HttpCancel,
    ) -> Result<Arc<Self>, ErrorCode> {
        if artifact.format != "cube" || artifact.size() > 4 << 20 {
            return Err(ErrorCode::Unsupported);
        }
        let _scratch = ledger.reserve(artifact.owner(), &[(Resource::HostBytes, 8 << 20)])?;
        let mut bytes = vec![0; artifact.size() as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            let n = artifact.read_at(offset as u64, &mut bytes[offset..], cancel)?;
            if n == 0 {
                return Err(ErrorCode::IoError);
            }
            offset += n;
        }
        let dimension = validate(&bytes)?;
        Ok(Arc::new(Self {
            artifact,
            dimension,
        }))
    }
    pub(crate) fn filter(&self) -> Result<String, ErrorCode> {
        let path = self.artifact.path();
        let path = path.to_str().ok_or(ErrorCode::Unsupported)?;
        #[cfg(windows)]
        let path = {
            let stripped = path
                .strip_prefix(r"\\?\")
                .filter(|s| !s.starts_with("UNC\\") && s.len() < 240)
                .unwrap_or(path);
            stripped.replace('\\', "/")
        };
        let option = escape(path.as_ref(), ":\\'");
        let graph = format!(
            "lut3d=file={}:interp=trilinear",
            escape(&option, "[]=;,\\'")
        );
        // mpv's length-delimited graph value encloses the entire generated
        // libavfilter graph, including commas or brackets in a host temp path.
        Ok(format!(
            "@iris-monitor-lut:lavfi=graph=%{}%{}",
            graph.len(),
            graph
        ))
    }
}

impl crate::player::MonitorLutFilter for MonitorLut {
    fn filter_spec(&self) -> anyhow::Result<String> {
        self.filter()
            .map_err(|e| anyhow::anyhow!("invalid LUT preview: {e:?}"))
    }
}
fn escape(value: &str, special: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if special.contains(ch) || ch.is_ascii_whitespace() {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}
fn triple(fields: &[&str]) -> Result<[f32; 3], ErrorCode> {
    if fields.len() != 3 {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut out = [0_f32; 3];
    for (i, s) in fields.iter().enumerate() {
        out[i] = s.parse().map_err(|_| ErrorCode::InvalidArgument)?;
        if !out[i].is_finite() || !(-16. ..=16.).contains(&out[i]) {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    Ok(out)
}
fn validate(bytes: &[u8]) -> Result<u32, ErrorCode> {
    let text = std::str::from_utf8(bytes).map_err(|_| ErrorCode::InvalidArgument)?;
    if text.contains('\0') {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut size = None::<u32>;
    let mut min = None;
    let mut max = None;
    let mut rows = 0usize;
    let mut title = false;
    for line in text.lines() {
        if line.len() > 512 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        match fields[0] {
            "TITLE" if !title && rows == 0 => {
                title = true;
                if !line[5..].trim_start().starts_with('"') || !line.ends_with('"') {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            "LUT_3D_SIZE" if size.is_none() && rows == 0 && fields.len() == 2 => {
                let n = fields[1]
                    .parse::<u32>()
                    .map_err(|_| ErrorCode::InvalidArgument)?;
                if !(2..=65).contains(&n) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                size = Some(n);
            }
            "DOMAIN_MIN" if min.is_none() && rows == 0 => min = Some(triple(&fields[1..])?),
            "DOMAIN_MAX" if max.is_none() && rows == 0 => max = Some(triple(&fields[1..])?),
            _ => {
                if size.is_none() {
                    return Err(ErrorCode::Unsupported);
                }
                triple(&fields)?;
                rows += 1;
                if rows > 65usize.pow(3) {
                    return Err(ErrorCode::QuotaExceeded);
                }
            }
        }
    }
    let size = size.ok_or(ErrorCode::Unsupported)?;
    let min = min.unwrap_or([0.; 3]);
    let max = max.unwrap_or([1.; 3]);
    if rows != (size as usize).pow(3)
        || min
            .into_iter()
            .zip(max)
            .any(|(a, b)| !(b - a).is_finite() || b - a < 1e-4)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cube_rejects_unknown_directives_bad_dimensions_and_nonfinite_samples() {
        let cube = format!("TITLE \"preview\"\nLUT_3D_SIZE 2\n{}", "0 1 0\n".repeat(8));
        assert_eq!(validate(cube.as_bytes()), Ok(2));
        for value in [
            cube.replace("0 1 0", "nan 1 0"),
            cube.replace("SIZE 2", "SIZE 65"),
            cube.replace("TITLE", "INCLUDE"),
            format!("{cube}0 0 0\n"),
        ] {
            assert!(validate(value.as_bytes()).is_err());
        }
        assert_eq!(escape("a':b\\c", ":\\'"), "a\\'\\:b\\\\c");
    }
}
