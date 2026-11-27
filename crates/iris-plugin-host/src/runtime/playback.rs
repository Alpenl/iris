use super::Control;
use crate::{
    actor::Status,
    instance::DetachedBlob,
    stream::prepare::{GuestFactory, Playback, Preparation},
};
use iris_api::v2::{Capability, ErrorCode, Generation, Id, stream::StreamSpec};
use iris_net::http_client::HttpCancel;
use std::collections::BTreeMap;

pub struct PlaybackPermit {
    preparation: Preparation,
    media: HttpCancel,
}
impl Control {
    /// Host source-request validation precedes this admission. Only this
    /// installation's grant/module can be used for the returned playback.
    pub fn playback_permit(
        &self,
        window: Id,
        generation: Generation,
    ) -> Result<PlaybackPermit, ErrorCode> {
        self.live()?;
        if let Some(actor) = &self.actor {
            match actor.status() {
                Status::Failed(error) => return Err(error),
                Status::Stopping | Status::Stopped => return Err(ErrorCode::Cancelled),
                _ => {}
            }
        }
        if !self.installed.grants.contains(&Capability::MediaSource)
            && !self.installed.grants.contains(&Capability::UrlResolver)
        {
            return Err(ErrorCode::Denied);
        }
        let (_, state) = self
            .window
            .as_ref()
            .filter(|(id, _)| *id == window)
            .ok_or(ErrorCode::StaleHandle)?;
        if state.generation != generation {
            return Err(ErrorCode::StaleHandle);
        }
        let guest = self.module.as_ref().map(|module| GuestFactory {
            engine: self.setup.resources.engine.clone(),
            module: module.clone(),
            capabilities: self.installed.grants.clone(),
            io: self.setup.resources.io.clone(),
            wake: self.setup.wake.clone(),
        });
        Ok(PlaybackPermit {
            preparation: Preparation {
                network: self.network.clone(),
                ledger: self.setup.resources.ledger.clone(),
                generation,
                guest,
            },
            media: state.media.clone(),
        })
    }
}
impl PlaybackPermit {
    /// Background worker only. Task cancellation ends with preparation; the
    /// ready relay remains bound to installation/window/media lifetime.
    pub fn prepare(
        self,
        spec: &StreamSpec,
        blobs: &BTreeMap<Id, DetachedBlob>,
        cancel: &HttpCancel,
    ) -> Result<Playback, ErrorCode> {
        let preparing = self.media.child().map_err(|_| ErrorCode::Cancelled)?;
        cancel.link(&preparing).map_err(|_| ErrorCode::Cancelled)?;
        let playback = self.preparation.prepare(spec, blobs, &preparing)?;
        self.media
            .link(playback.relay.session().cancellation())
            .map_err(|_| ErrorCode::Cancelled)?;
        if preparing.is_cancelled() {
            playback.relay.stop();
            return Err(ErrorCode::Cancelled);
        }
        Ok(playback)
    }
}
