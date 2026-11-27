//! Trusted UI capabilities for one installation/window/media. No guest path or
//! authority is promoted through this interface; callers retain Binding checks.
use super::{Control, Resources};
use crate::{
    enhancement::AttachmentRegistry,
    files::FileExecutor,
    ledger::{Reservation, Resource},
    network::NetworkGrant,
};
use futures::channel::oneshot;
use iris_api::v2::{Capability, ErrorCode, Generation, Id, Role};
use iris_net::http_client::HttpCancel;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone)]
pub struct WindowAccess {
    pub instance: Id,
    pub window: Id,
    pub generation: Generation,
    pub owner: String,
    pub capabilities: BTreeSet<Capability>,
    pub network: NetworkGrant,
    pub files: FileExecutor,
    pub resources: Resources,
    pub cancel: HttpCancel,
    pub attachments: Arc<AttachmentRegistry>,
}
pub struct BackgroundResult<T> {
    pub value: T,
    _lease: Reservation,
}
impl WindowAccess {
    pub fn request_cancel(&self, parent: &HttpCancel) -> Result<HttpCancel, ErrorCode> {
        let child = self.cancel.child().map_err(|_| ErrorCode::Cancelled)?;
        parent.link(&child).map_err(|_| ErrorCode::Cancelled)?;
        Ok(child)
    }
    pub fn check(&self, generation: Generation, capability: Capability) -> Result<(), ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if self.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        if !self.capabilities.contains(&capability) {
            return Err(ErrorCode::Denied);
        }
        Ok(())
    }
    /// Bounded shared control-I/O workers. Reservations live through completion
    /// delivery, including if the receiving window has already disappeared.
    pub fn background<T: Send + 'static>(
        &self,
        work: impl FnOnce(Self) -> Result<T, ErrorCode> + Send + 'static,
    ) -> Result<oneshot::Receiver<Result<BackgroundResult<T>, ErrorCode>>, ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let lease = self.resources.ledger.reserve(
            &self.owner,
            &[(Resource::Task, 1), (Resource::HostBytes, 2 << 20)],
        )?;
        let access = self.clone();
        let (tx, rx) = oneshot::channel();
        self.resources.io.submit(
            Role::Control,
            &self.owner,
            Box::new(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if access.cancel.is_cancelled() {
                        Err(ErrorCode::Cancelled)
                    } else {
                        work(access)
                    }
                }))
                .unwrap_or(Err(ErrorCode::Internal));
                let _ = tx.send(result.map(|value| BackgroundResult {
                    value,
                    _lease: lease,
                }));
            }),
        )?;
        Ok(rx)
    }
}
impl Control {
    pub(crate) fn window_access(&self, window: Id) -> Result<WindowAccess, ErrorCode> {
        self.live()?;
        let (_, state) = self
            .window
            .as_ref()
            .filter(|(id, _)| *id == window)
            .ok_or(ErrorCode::StaleHandle)?;
        Ok(WindowAccess {
            instance: self.control_id,
            window,
            generation: state.generation,
            owner: self.installed.installation_id.clone(),
            capabilities: self.installed.grants.clone(),
            network: self.network.clone(),
            files: state.files.clone(),
            resources: self.setup.resources.clone(),
            cancel: state.media.clone(),
            attachments: state.attachments.clone(),
        })
    }
}
