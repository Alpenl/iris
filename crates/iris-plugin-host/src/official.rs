//! Host-embedded official archives and pins. No installation-directory file can
//! declare itself official. Bootstrap runs on the coordinator's disk worker.
mod migration;
pub mod smoke;
use super::{
    installation::{Approval, InstallationEntry, Installed, Installer, Review},
    json_error,
    package::{Package, Provenance},
    runtime::Setup,
};
use iris_api::v2::ErrorCode;
use iris_base::rt::CancelToken;
use iris_core::media_uri::WEBDAV_PLUGIN_ID;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

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
struct Index {
    schema: u32,
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
    bytes: Arc<[u8]>,
}

pub(crate) struct OfficialUpgrade {
    pub plugin_id: String,
    pub review: Review,
}

pub(crate) struct BootstrapScan {
    pub entries: Vec<InstallationEntry>,
    pub pending_reviews: Vec<OfficialUpgrade>,
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
        let index: Index = iris_core::bounded_json::decode(INDEX, 16 * 1024).map_err(json_error)?;
        if index.schema != 1 || index.packages.len() != 2 {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut pins = BTreeMap::new();
        let mut entries = BTreeMap::new();
        for entry in index.packages {
            let raw: &[u8] = match entry.id.as_str() {
                "iris.guides" => GUIDES,
                WEBDAV_PLUGIN_ID => WEBDAV,
                _ => return Err(ErrorCode::InvalidArgument),
            };
            if entry.file != format!("{}.irisplugin", entry.id) || entry.version.is_empty() {
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
                    bytes: Arc::from(raw),
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
    pub(crate) fn migrate_installed(
        &self,
        installed: &Installed,
        setup: &Setup,
    ) -> Result<(), ErrorCode> {
        if matches!(
            installed.plugin_id.as_str(),
            "iris.guides" | WEBDAV_PLUGIN_ID
        ) {
            migration::migrate(&self.legacy_root, installed, setup)
        } else {
            Ok(())
        }
    }
    /// A failed official migration disables that installation's load; unrelated
    /// user packages can still load. No guest observes partially migrated state.
    pub fn entries(
        &self,
        installer: Installer,
        setup: &Setup,
    ) -> Result<Vec<InstallationEntry>, ErrorCode> {
        self.scan(installer, setup, &CancelToken::new())
            .map(|scan| scan.entries)
    }
    pub(crate) fn scan(
        &self,
        mut installer: Installer,
        setup: &Setup,
        cancel: &CancelToken,
    ) -> Result<BootstrapScan, ErrorCode> {
        let mut failures = BTreeMap::new();
        let mut pending_reviews = Vec::new();
        for (id, pin) in &self.entries {
            match self.ensure(&mut installer, setup, id, &pin.bytes, cancel) {
                Ok(None) => {}
                Ok(Some(upgrade)) => {
                    // Hold the expanded official upgrade behind explicit user review
                    // rather than silently enlarging grants or silently bricking it.
                    failures.insert(id.clone(), ErrorCode::Denied);
                    pending_reviews.push(upgrade);
                }
                Err(error) => {
                    failures.insert(id.clone(), error);
                }
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
        Ok(BootstrapScan {
            entries,
            pending_reviews,
        })
    }
    fn ensure(
        &self,
        installer: &mut Installer,
        setup: &Setup,
        id: &str,
        bytes: &[u8],
        cancel: &CancelToken,
    ) -> Result<Option<OfficialUpgrade>, ErrorCode> {
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
        // Expanded official manifests route through explicit host review/approval.
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
                let review =
                    Review::from_bundled(bytes.to_vec(), installer, &setup.resources, cancel)?;
                return Ok(Some(OfficialUpgrade {
                    plugin_id: id.to_owned(),
                    review,
                }));
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
        self.migrate_installed(&installed, setup)?;
        Ok(None)
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
