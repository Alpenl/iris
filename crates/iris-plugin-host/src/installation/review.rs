use super::{Approval, ArchivedIdentity, Installed, Installer};
use crate::{
    engine::CompiledModule,
    instance::next_id,
    ledger::{Reservation, Resource},
    package::{Package, Provenance},
    services::Resources,
};
use iris_api::v2::{Capability, ErrorCode, Manifest};
use iris_base::rt::{CancelToken, Deadline};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

/// A reviewed archive is immutable in memory. Compilation checks the module
/// without executing start/init before the user authorizes installation.
pub struct Review {
    manifest: Manifest,
    digest: [u8; 32],
    previous: Option<Installed>,
    archived: Option<ArchivedIdentity>,
    bytes: Vec<u8>,
    bundled: bool,
    _module: Option<Arc<CompiledModule>>,
    expires: Deadline,
    _lease: Reservation,
}
impl Review {
    /// Background worker only. The coordinator bounds pending reviews to two.
    pub fn read(
        path: &Path,
        installer: &Installer,
        resources: &Resources,
        cancel: &CancelToken,
    ) -> Result<Self, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let bytes = Installer::read_candidate(path)?;
        Self::from_bytes(bytes, installer, resources, cancel, false)
    }
    pub(crate) fn from_bundled(
        bytes: Vec<u8>,
        installer: &Installer,
        resources: &Resources,
        cancel: &CancelToken,
    ) -> Result<Self, ErrorCode> {
        Self::from_bytes(bytes, installer, resources, cancel, true)
    }
    fn from_bytes(
        bytes: Vec<u8>,
        installer: &Installer,
        resources: &Resources,
        cancel: &CancelToken,
        bundled: bool,
    ) -> Result<Self, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let owner = format!("package-review-{}", next_id()?);
        let mut lease = resources
            .ledger
            .reserve(&owner, &[(Resource::HostBytes, 128 << 20)])?;
        let provenance = if bundled {
            Provenance::Bundled(installer.bundled_pins())
        } else {
            Provenance::User
        };
        let package = Package::from_zip(&bytes, provenance)?;
        let module = package
            .wasm()
            .map(|wasm| resources.engine.compile(wasm, cancel))
            .transpose()?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let previous = installer.record(&package.manifest.id)?;
        let archived = installer.archived(&package.manifest.id).ok().flatten();
        let manifest = package.manifest.clone();
        let digest = package.digest;
        drop(package);
        lease.replace(&[(Resource::HostBytes, bytes.capacity() as u64 + (2 << 20))])?;
        Ok(Self {
            manifest,
            digest,
            previous,
            archived,
            bytes,
            bundled,
            _module: module,
            expires: Deadline::after(Duration::from_secs(300)),
            _lease: lease,
        })
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn previous(&self) -> Option<&Installed> {
        self.previous.as_ref()
    }
    pub fn archived(&self) -> Option<&ArchivedIdentity> {
        self.archived.as_ref()
    }
    pub(crate) fn is_bundled(&self) -> bool {
        self.bundled
    }
    pub(crate) fn refresh_expiry(&mut self) {
        self.expires = Deadline::after(Duration::from_secs(300));
    }
    /// Keep identity only after the UI explicitly offers that choice alongside
    /// both content digests and the requested grants. Reinstall gets a new scope
    /// unless the user explicitly approves restoring an archived identity.
    pub(crate) fn approve_with_cleanup(
        self,
        installer: &mut Installer,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    ) -> Result<(Installed, Vec<String>), ErrorCode> {
        if self.expires.is_expired() {
            return Err(ErrorCode::StaleHandle);
        }
        let previous = if keep_identity {
            if let Some(prev) = self.previous.as_ref() {
                Some((prev.installation_id.clone(), prev.digest))
            } else if let Some(arch) = self.archived.as_ref() {
                Some((arch.installation_id.clone(), arch.digest))
            } else {
                return Err(ErrorCode::StaleHandle);
            }
        } else {
            None
        };
        installer.install_reviewed(
            &self.bytes,
            Approval {
                digest: self.digest,
                grants,
                network: self.manifest.network,
                previous,
            },
            self.bundled,
            Some(&self.previous),
            Some(&self.archived),
        )
    }
}

#[cfg(test)]
mod tests;
