//! File capabilities are issued by trusted host interaction and consumed by
//! typed tasks. No guest-supplied path is accepted by this module.
mod artifact;
mod export;
mod hashing;
pub mod identity;
use super::{
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
};
pub use artifact::{Artifact, ArtifactWriter, TempRoot};
pub use export::extension as export_extension;
pub use export::{ExportAudit, ExportGrant, ExportState};
pub use hashing::hash_media;
use identity::PinnedFile;
use iris_api::v2::{ErrorCode, Generation, Id};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    Slice,
    ExtractAudio,
}
pub struct MediaInput {
    pub file: Arc<PinnedFile>,
    pub media: Id,
    pub generation: Generation,
    action: InputAction,
    expires: Instant,
    _lease: Reservation,
}
pub struct FileGrants {
    owner: String,
    ledger: Ledger,
    inputs: Mutex<BTreeMap<Id, MediaInput>>,
    epoch: std::sync::atomic::AtomicU64,
}
impl FileGrants {
    pub fn new(owner: String, ledger: Ledger) -> Self {
        Self {
            owner,
            ledger,
            inputs: Mutex::new(BTreeMap::new()),
            epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }
    /// Called on a background host action after the user approves this input and
    /// operation. Permission external_tool alone is insufficient.
    pub fn approve_input(
        &self,
        path: &Path,
        media: Id,
        generation: Generation,
        action: InputAction,
    ) -> Result<Id, ErrorCode> {
        self.approve_input_scoped(
            path,
            media,
            generation,
            action,
            &crate::vfs::HttpCancel::default(),
        )
    }
    pub fn approve_input_scoped(
        &self,
        path: &Path,
        media: Id,
        generation: Generation,
        action: InputAction,
        cancel: &crate::vfs::HttpCancel,
    ) -> Result<Id, ErrorCode> {
        if media.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Acquire);
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, input| input.expires > Instant::now());
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
        )?;
        let file = Arc::new(PinnedFile::open(path)?);
        let id = Id(next_id()?);
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if self.epoch.load(std::sync::atomic::Ordering::Acquire) != epoch {
            return Err(ErrorCode::StaleHandle);
        }
        inputs.retain(|_, v| v.expires > Instant::now());
        inputs.insert(
            id,
            MediaInput {
                file,
                media,
                generation,
                action,
                expires: Instant::now() + Duration::from_secs(300),
                _lease: lease,
            },
        );
        Ok(id)
    }
    pub fn close_input(&self, id: Id) {
        self.inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
    pub fn consume_input(
        &self,
        owner: &str,
        id: Id,
        generation: Generation,
        action: InputAction,
    ) -> Result<MediaInput, ErrorCode> {
        if owner != self.owner {
            return Err(ErrorCode::Denied);
        }
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        let input = inputs.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if input.generation != generation
            || input.action != action
            || input.expires <= Instant::now()
        {
            return Err(ErrorCode::StaleHandle);
        }
        inputs.remove(&id).ok_or(ErrorCode::StaleHandle)
    }
    pub fn revoke(&self) {
        let mut inputs = self.inputs.lock().unwrap_or_else(|e| e.into_inner());
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        inputs.clear();
    }
}

pub(crate) fn random_name() -> Result<String, ErrorCode> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests;
