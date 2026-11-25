//! Content-addressed packages plus a single atomic installation record. Grant
//! confirmation is supplied by trusted host interaction, never by a guest form.
mod review;
use super::{
    ledger::{Ledger, Resource},
    package::{PACKAGE_LIMIT, Package, Provenance},
};
use iris_api::v2::{Capability, ErrorCode, MESSAGE_LIMIT, Manifest, NetworkDeclaration};
use iris_plugin_sdk::json::decode;
pub use review::Review;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub plugin_id: String,
    pub installation_id: String,
    pub digest: [u8; 32],
    pub object: String,
    pub grants: BTreeSet<Capability>,
    pub network: NetworkDeclaration,
}

/// Optional identity preserved when a user uninstalls a plugin with credential/state
/// retention enabled. Reinstall only adopts it after explicit host consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedIdentity {
    pub plugin_id: String,
    pub installation_id: String,
    pub digest: [u8; 32],
}

/// A host approval pins the exact reviewed contents and optional prior identity.
/// Reinstallation without an explicit previous identity always gets a fresh ID.
pub struct Approval {
    pub digest: [u8; 32],
    pub grants: BTreeSet<Capability>,
    pub network: NetworkDeclaration,
    pub previous: Option<(String, [u8; 32])>,
}

#[derive(Clone)]
pub struct Installer {
    root: PathBuf,
    ledger: Ledger,
    bundled: BTreeMap<String, [u8; 32]>,
}
pub struct InstallationEntry {
    pub plugin_id: String,
    pub record: Result<Installed, ErrorCode>,
}
impl Installer {
    pub fn new(root: PathBuf, ledger: Ledger) -> Self {
        Self {
            root,
            ledger,
            bundled: BTreeMap::new(),
        }
    }
    /// The index is embedded by the host distribution, never read from a user
    /// package or installation record. User installation still uses User provenance.
    pub fn with_bundled(
        root: PathBuf,
        ledger: Ledger,
        bundled: BTreeMap<String, [u8; 32]>,
    ) -> Self {
        Self {
            root,
            ledger,
            bundled,
        }
    }

    pub fn install(&mut self, bytes: &[u8], approval: Approval) -> Result<Installed, ErrorCode> {
        self.install_from(bytes, approval, Provenance::User, None, None)
            .map(|(installed, _)| installed)
    }
    pub fn install_bundled(
        &mut self,
        bytes: &[u8],
        approval: Approval,
    ) -> Result<Installed, ErrorCode> {
        self.install_from(
            bytes,
            approval,
            Provenance::Bundled(&self.bundled),
            None,
            None,
        )
        .map(|(installed, _)| installed)
    }

    pub fn is_bundled(&self, id: &str) -> bool {
        self.bundled.contains_key(id) || id.to_ascii_lowercase().starts_with("iris.")
    }

    pub fn archived(&self, id: &str) -> Result<Option<ArchivedIdentity>, ErrorCode> {
        if !iris_api::is_valid_id(id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let path = self.root.join("archived").join(format!("{id}.json"));
        let file = match crate::fs_util::retry_sharing_violation(|| std::fs::File::open(&path)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ErrorCode::IoError),
        };
        let mut bytes = Vec::new();
        file.take(MESSAGE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ErrorCode::IoError)?;
        let record: ArchivedIdentity = decode(&bytes, MESSAGE_LIMIT)?;
        if record.plugin_id != id || !valid_identity(&record.installation_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Some(record))
    }

    /// Removes a user-installed plugin record and its immutable archive object.
    /// Bundled official plugins (`iris.*`) cannot be uninstalled.
    /// When `keep_identity` is true, archives `(plugin_id, installation_id, digest)`
    /// so a future reinstall can explicitly opt in to restore preserved state/secrets.
    pub fn uninstall(&self, id: &str, keep_identity: bool) -> Result<Option<Installed>, ErrorCode> {
        if !iris_api::is_valid_id(id) {
            return Err(ErrorCode::InvalidArgument);
        }
        if self.is_bundled(id) {
            return Err(ErrorCode::Denied);
        }
        let _lock = crate::fs_util::lock_file_with_timeout(
            &self.root.join("install.lock"),
            std::time::Duration::from_millis(250),
        )
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let record_path = self.root.join("installed").join(format!("{id}.json"));
        let prior = self.record(id).ok().flatten();
        match std::fs::remove_file(&record_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ErrorCode::IoError),
        }
        let archived_dir = self.root.join("archived");
        let archived_path = archived_dir.join(format!("{id}.json"));
        if keep_identity {
            if let Some(installed) = &prior {
                let marker = ArchivedIdentity {
                    plugin_id: installed.plugin_id.clone(),
                    installation_id: installed.installation_id.clone(),
                    digest: installed.digest,
                };
                let json = serde_json::to_vec(&marker).map_err(|_| ErrorCode::Internal)?;
                std::fs::create_dir_all(&archived_dir).map_err(|_| ErrorCode::IoError)?;
                crate::fs_util::atomic_write_private(&archived_path, &json)
                    .map_err(|_| ErrorCode::IoError)?;
            }
        } else {
            let _ = std::fs::remove_file(&archived_path);
        }
        if let Some(installed) = &prior {
            let _ = std::fs::remove_file(self.root.join("objects").join(&installed.object));
        }
        Ok(prior)
    }
    fn install_from(
        &self,
        bytes: &[u8],
        approval: Approval,
        provenance: Provenance<'_>,
        expected: Option<&Option<Installed>>,
        expected_archived: Option<&Option<ArchivedIdentity>>,
    ) -> Result<(Installed, Option<String>), ErrorCode> {
        let _memory = self
            .ledger
            .reserve("package-installer", &[(Resource::HostBytes, 128 << 20)])?;
        let package = Package::from_zip(bytes, provenance)?;
        validate_approval(&package.manifest, &approval)?;
        if package.digest != approval.digest {
            return Err(ErrorCode::Denied);
        }
        let _lock = crate::fs_util::lock_file_with_timeout(
            &self.root.join("install.lock"),
            std::time::Duration::from_millis(250),
        )
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let prior = self.record(&package.manifest.id)?;
        let archived = self.archived(&package.manifest.id).ok().flatten();
        if expected.is_some_and(|expected| *expected != prior)
            || expected_archived.is_some_and(|expected| *expected != archived)
        {
            return Err(ErrorCode::StaleHandle);
        }
        let (installation_id, superseded_archived) = match (&approval.previous, &prior, &archived) {
            (Some((identity, digest)), Some(prior), _)
                if &prior.installation_id == identity && &prior.digest == digest =>
            {
                (
                    identity.clone(),
                    archived
                        .as_ref()
                        .map(|a| a.installation_id.clone())
                        .filter(|a| a != identity),
                )
            }
            (Some((identity, digest)), None, Some(arch))
                if &arch.installation_id == identity && &arch.digest == digest =>
            {
                (identity.clone(), None)
            }
            (Some(_), _, _) => return Err(ErrorCode::StaleHandle),
            (None, _, arch) => (
                random_identity()?,
                arch.as_ref().map(|a| a.installation_id.clone()),
            ),
        };
        let object = format!("{}-{}.irisplugin", installation_id, hex(&package.digest));
        let objects = self.root.join("objects");
        let records = self.root.join("installed");
        std::fs::create_dir_all(&objects).map_err(|_| ErrorCode::IoError)?;
        std::fs::create_dir_all(&records).map_err(|_| ErrorCode::IoError)?;
        let destination = objects.join(&object);
        // Immutable object name; collisions are verified rather than overwritten.
        if destination.exists() {
            let existing = Package::read_zip(&destination, provenance)?;
            if existing.digest != package.digest {
                return Err(ErrorCode::Denied);
            }
        } else {
            let staging = objects.join(format!("{}.staging", random_identity()?));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let written = (|| -> std::io::Result<()> {
                let mut file = options.open(&staging)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&staging, &destination)
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&staging);
                return Err(ErrorCode::IoError);
            }
        }
        let record = Installed {
            plugin_id: package.manifest.id,
            installation_id,
            digest: package.digest,
            object,
            grants: approval.grants,
            network: approval.network,
        };
        let json = serde_json::to_vec(&record).map_err(|_| ErrorCode::Internal)?;
        if json.len() > MESSAGE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        // The old record still points to its valid immutable object if this fails.
        crate::fs_util::atomic_write_private(
            &records.join(format!("{}.json", record.plugin_id)),
            &json,
        )
        .map_err(|_| ErrorCode::IoError)?;
        let _ = std::fs::remove_file(
            self.root
                .join("archived")
                .join(format!("{}.json", record.plugin_id)),
        );
        Ok((record, superseded_archived))
    }

    pub fn record(&self, id: &str) -> Result<Option<Installed>, ErrorCode> {
        if !iris_api::is_valid_id(id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let path = self.root.join("installed").join(format!("{id}.json"));
        let file = match crate::fs_util::retry_sharing_violation(|| std::fs::File::open(&path)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ErrorCode::IoError),
        };
        let mut bytes = Vec::new();
        file.take(MESSAGE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ErrorCode::IoError)?;
        let record: Installed = decode(&bytes, MESSAGE_LIMIT)?;
        if record.plugin_id != id
            || !valid_identity(&record.installation_id)
            || record.object
                != format!(
                    "{}-{}.irisplugin",
                    record.installation_id,
                    hex(&record.digest)
                )
        {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Some(record))
    }

    pub fn load(&self, record: &Installed) -> Result<Package, ErrorCode> {
        let current = self
            .record(&record.plugin_id)?
            .ok_or(ErrorCode::StaleHandle)?;
        if current != *record {
            return Err(ErrorCode::StaleHandle);
        }
        let _memory = self.ledger.reserve(
            &format!("package-read-{}", record.installation_id),
            &[(Resource::HostBytes, 128 << 20)],
        )?;
        let package = Package::read_zip(
            &self.root.join("objects").join(current.object),
            if current.plugin_id.to_ascii_lowercase().starts_with("iris.") {
                Provenance::Bundled(&self.bundled)
            } else {
                Provenance::User
            },
        )?;
        if package.digest != current.digest {
            return Err(ErrorCode::Denied);
        }
        validate_approval(
            &package.manifest,
            &Approval {
                digest: current.digest,
                grants: current.grants,
                network: current.network,
                previous: None,
            },
        )?;
        package.account(&self.ledger, &current.installation_id)
    }

    /// Managed installation records only. One damaged record is reported on its
    /// own row and never interpreted as authorization to execute a package.
    pub fn entries(&self) -> Result<Vec<InstallationEntry>, ErrorCode> {
        let entries = match std::fs::read_dir(self.root.join("installed")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ErrorCode::IoError),
        };
        let mut names = BTreeSet::new();
        for (count, entry) in entries.enumerate() {
            if count >= 256 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let entry = entry.map_err(|_| ErrorCode::IoError)?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if !iris_api::is_valid_id(id) {
                continue;
            }
            if names.len() >= 64 {
                return Err(ErrorCode::QuotaExceeded);
            }
            names.insert(id.to_owned());
        }
        Ok(names
            .into_iter()
            .map(|plugin_id| {
                let record = self
                    .record(&plugin_id)
                    .and_then(|record| record.ok_or(ErrorCode::StaleHandle));
                InstallationEntry { plugin_id, record }
            })
            .collect())
    }

    pub(crate) fn read_prior_permissions(
        &self,
        record: &Installed,
    ) -> Option<BTreeSet<Capability>> {
        let path = self.root.join("objects").join(&record.object);
        let file = crate::fs_util::retry_sharing_violation(|| std::fs::File::open(&path)).ok()?;
        let bytes = super::package::read_bounded(file, PACKAGE_LIMIT).ok()?;
        let mut pins = BTreeMap::new();
        pins.insert(record.plugin_id.clone(), record.digest);
        let provenance = if record.plugin_id.to_ascii_lowercase().starts_with("iris.") {
            Provenance::Bundled(&pins)
        } else {
            Provenance::User
        };
        let package = Package::from_zip(&bytes, provenance).ok()?;
        (package.digest == record.digest)
            .then(|| package.manifest.permissions.into_iter().collect())
    }

    pub fn read_candidate(path: &Path) -> Result<Vec<u8>, ErrorCode> {
        let file = std::fs::File::open(path).map_err(|_| ErrorCode::IoError)?;
        super::package::read_bounded(file, PACKAGE_LIMIT)
    }
}

fn validate_approval(manifest: &Manifest, approval: &Approval) -> Result<(), ErrorCode> {
    if approval
        .grants
        .iter()
        .any(|cap| !manifest.permissions.contains(cap))
        || approval.network != manifest.network
    {
        return Err(ErrorCode::Denied);
    }
    Ok(())
}
fn random_identity() -> Result<String, ErrorCode> {
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(hex(&bytes))
}
pub(crate) fn valid_identity(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
