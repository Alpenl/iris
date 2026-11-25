use super::{Approval, ArchivedIdentity, Installed, Installer};
use crate::plugin_v2::{
    engine::CompiledModule,
    instance::next_id,
    ledger::{Reservation, Resource},
    package::{Package, Provenance},
    runtime::Resources,
};
use iris_api::v2::{Capability, ErrorCode, Manifest};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A reviewed archive is immutable in memory. Compilation checks the module
/// without executing start/init before the user authorizes installation.
pub struct Review {
    manifest: Manifest,
    digest: [u8; 32],
    previous: Option<Installed>,
    archived: Option<ArchivedIdentity>,
    bytes: Vec<u8>,
    _module: Option<Arc<CompiledModule>>,
    expires: Instant,
    _lease: Reservation,
}
impl Review {
    /// Background worker only. The coordinator bounds pending reviews to two.
    pub fn read(
        path: &Path,
        installer: &Installer,
        resources: &Resources,
        cancel: &AtomicBool,
    ) -> Result<Self, ErrorCode> {
        if cancel.load(Ordering::Acquire) {
            return Err(ErrorCode::Cancelled);
        }
        let owner = format!("package-review-{}", next_id()?);
        let mut lease = resources
            .ledger
            .reserve(&owner, &[(Resource::HostBytes, 128 << 20)])?;
        let bytes = Installer::read_candidate(path)?;
        let package = Package::from_zip(&bytes, Provenance::User)?;
        let module = package
            .wasm()
            .map(|wasm| resources.engine.compile(wasm, cancel))
            .transpose()?;
        if cancel.load(Ordering::Acquire) {
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
            _module: module,
            expires: Instant::now() + Duration::from_secs(300),
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
    /// Keep identity only after the UI explicitly offers that choice alongside
    /// both content digests and the requested grants. Reinstall gets a new scope
    /// unless the user explicitly approves restoring an archived identity.
    pub fn approve(
        self,
        installer: &mut Installer,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    ) -> Result<Installed, ErrorCode> {
        self.approve_with_cleanup(installer, grants, keep_identity)
            .map(|(installed, _)| installed)
    }
    pub(crate) fn approve_with_cleanup(
        self,
        installer: &mut Installer,
        grants: BTreeSet<Capability>,
        keep_identity: bool,
    ) -> Result<(Installed, Option<String>), ErrorCode> {
        if Instant::now() >= self.expires {
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
        installer.install_from(
            &self.bytes,
            Approval {
                digest: self.digest,
                grants,
                network: self.manifest.network,
                previous,
            },
            Provenance::User,
            Some(&self.previous),
            Some(&self.archived),
        )
    }
}

#[cfg(test)]
mod tests;
