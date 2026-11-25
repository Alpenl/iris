//! Host-embedded official archives and pins. No installation-directory file can
//! declare itself official. Bootstrap runs on the coordinator's disk worker.
mod migration;
pub mod smoke;
use super::{
    installation::{Approval, InstallationEntry, Installer},
    package::{Package, Provenance},
    runtime::Setup,
};
use iris_api::v2::ErrorCode;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

const GUIDES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../Resources/Plugins/iris.guides.irisplugin"
));
const WEBDAV: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../Resources/Plugins/iris.webdav.irisplugin"
));
const INDEX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../Resources/Plugins/index.json"
));
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema: u32,
    source_sha256: String,
    packages: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    version: String,
    file: String,
    sha256: String,
    content_sha256: String,
}

#[derive(Clone)]
struct Pin {
    sha256: [u8; 32],
    content_sha256: [u8; 32],
    version: String,
}

#[derive(Clone)]
pub struct Bootstrap {
    legacy_root: PathBuf,
    pins: BTreeMap<String, [u8; 32]>,
    entries: BTreeMap<String, Pin>,
}
impl Bootstrap {
    /// Only parses a small embedded index; no filesystem, hashing, or compilation.
    pub fn embedded(legacy_root: PathBuf) -> Result<Self, ErrorCode> {
        let index: Index = crate::plugin_v2::json::decode(INDEX, 16 * 1024)?;
        if index.schema != 1 || index.packages.len() != 2 || hex(&index.source_sha256).is_err() {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut pins = BTreeMap::new();
        let mut entries = BTreeMap::new();
        for entry in index.packages {
            if !matches!(entry.id.as_str(), "iris.guides" | "iris.webdav")
                || entry.file != format!("{}.irisplugin", entry.id)
                || entry.version.is_empty()
            {
                return Err(ErrorCode::InvalidArgument);
            }
            let sha256 = hex(&entry.sha256)?;
            let content_sha256 = hex(&entry.content_sha256)?;
            if pins.insert(entry.id.clone(), content_sha256).is_some() {
                return Err(ErrorCode::InvalidArgument);
            }
            entries.insert(
                entry.id,
                Pin {
                    sha256,
                    content_sha256,
                    version: entry.version,
                },
            );
        }
        Ok(Self {
            legacy_root,
            pins,
            entries,
        })
    }
    pub fn pins(&self) -> BTreeMap<String, [u8; 32]> {
        self.pins.clone()
    }
    /// A failed official migration disables that installation's load; unrelated
    /// user packages can still load. No guest observes partially migrated state.
    pub fn entries(
        &self,
        mut installer: Installer,
        setup: &Setup,
    ) -> Result<Vec<InstallationEntry>, ErrorCode> {
        let mut failures = BTreeMap::new();
        for (id, bytes) in [("iris.guides", GUIDES), ("iris.webdav", WEBDAV)] {
            if let Err(error) = self.ensure(&mut installer, setup, id, bytes) {
                failures.insert(id.to_owned(), error);
            }
        }
        let mut entries = installer.entries()?;
        for (id, error) in failures {
            if let Some(entry) = entries.iter_mut().find(|e| e.plugin_id == id) {
                entry.record = Err(error);
            } else {
                entries.push(InstallationEntry {
                    plugin_id: id,
                    record: Err(error),
                });
            }
        }
        Ok(entries)
    }
    fn ensure(
        &self,
        installer: &mut Installer,
        setup: &Setup,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), ErrorCode> {
        let _memory = setup.resources.ledger.reserve(
            "official-bootstrap",
            &[(super::ledger::Resource::HostBytes, 64 << 20)],
        )?;
        let pin = self.entries.get(id).ok_or(ErrorCode::Denied)?;
        if ring::digest::digest(&ring::digest::SHA256, bytes).as_ref() != pin.sha256 {
            return Err(ErrorCode::Denied);
        }
        let package = Package::from_zip(bytes, Provenance::Bundled(&self.pins))?;
        if package.manifest.id != id
            || package.digest != pin.content_sha256
            || package.manifest.version != pin.version
        {
            return Err(ErrorCode::Denied);
        }
        let previous = installer.record(id)?;
        // An application update does not silently enlarge an existing grant.
        // Narrowed user grants remain valid when the official manifest itself
        // did not add new requested capabilities beyond the previously reviewed object.
        if let Some(p) = previous.as_ref()
            && p.digest != package.digest
        {
            let prior_declared = installer
                .read_prior_permissions(p)
                .unwrap_or_else(|| p.grants.clone());
            if p.network != package.manifest.network
                || package
                    .manifest
                    .permissions
                    .iter()
                    .any(|cap| !p.grants.contains(cap) && !prior_declared.contains(cap))
            {
                return Err(ErrorCode::Denied);
            }
        }
        let approval = || Approval {
            digest: package.digest,
            grants: previous.as_ref().map_or_else(
                || package.manifest.permissions.iter().copied().collect(),
                |p| {
                    p.grants
                        .iter()
                        .copied()
                        .filter(|c| package.manifest.permissions.contains(c))
                        .collect()
                },
            ),
            network: package.manifest.network.clone(),
            previous: previous
                .as_ref()
                .map(|p| (p.installation_id.clone(), p.digest)),
        };
        let installed = match previous.as_ref() {
            Some(record) if record.digest == package.digest => {
                // Verify the persisted object too; a missing/tampered package is
                // surfaced, never silently trusted because the record matched.
                if installer.load(record).is_err() {
                    // Reconstruct a missing immutable object. Existing corrupt
                    // objects still fail Installer's collision verification.
                    installer.install_bundled(bytes, approval())?
                } else {
                    record.clone()
                }
            }
            _ => installer.install_bundled(bytes, approval())?,
        };
        migration::migrate(&self.legacy_root, &installed, setup)
    }
}
fn hex(value: &str) -> Result<[u8; 32], ErrorCode> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .map_err(|_| ErrorCode::InvalidArgument)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
