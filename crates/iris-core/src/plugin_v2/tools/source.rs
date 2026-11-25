use crate::{
    plugin_v2::{
        actor::ReadResult,
        files::{Artifact, identity::PinnedFile},
        instance::Blob,
        ledger::{Ledger, Resource},
        network::NetworkGrant,
        stream::{
            reader::{Reader, check_read},
            relay::{BoundRelay, Relay},
            remote::RemoteFactory,
            session::{StreamSession, new_session_id},
        },
    },
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, Generation, NetworkDeclaration};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Clone)]
pub(super) enum Data {
    File(Arc<PinnedFile>),
    Artifact(Arc<Artifact>),
}
impl Data {
    pub fn size(&self) -> u64 {
        match self {
            Self::File(f) => f.initial.size,
            Self::Artifact(f) => f.size(),
        }
    }
    pub fn read(
        &self,
        offset: u64,
        bytes: &mut [u8],
        cancel: &HttpCancel,
    ) -> Result<usize, ErrorCode> {
        match self {
            Self::File(f) => f.read_at(offset, bytes, cancel),
            Self::Artifact(f) => f.read_at(offset, bytes, cancel),
        }
    }
}
struct FileReader {
    data: Data,
    ledger: Ledger,
    owner: String,
}
impl Reader for FileReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(Some(self.data.size()))
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        let len = (len as u64).min(
            self.data
                .size()
                .checked_sub(offset)
                .ok_or(ErrorCode::RangeInvalid)?,
        ) as usize;
        let _scratch = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, len as u64)])?;
        let mut bytes = vec![0; len];
        let n = self.data.read(offset, &mut bytes, cancel)?;
        bytes.truncate(n);
        Ok(ReadResult {
            blob: Arc::new(Blob::new(&self.ledger, &self.owner, bytes)?),
            eof: offset + n as u64 == self.data.size(),
        })
    }
}
pub(super) struct InputRelay {
    pub url: String,
    relay: Relay,
}
impl InputRelay {
    pub fn open(
        data: Data,
        owner: &str,
        ledger: &Ledger,
        generation: Generation,
        cancel: &HttpCancel,
    ) -> Result<Self, ErrorCode> {
        let factory = RemoteFactory::new(
            NetworkGrant::new(
                owner.into(),
                BTreeSet::new(),
                NetworkDeclaration::default(),
                ledger,
            )?,
            ledger.clone(),
        )?;
        let bound = BoundRelay::bind()?;
        let session = StreamSession::new(
            new_session_id()?,
            generation,
            bound.authority()?,
            factory,
            None,
        )?;
        cancel
            .link(session.cancellation())
            .map_err(|_| ErrorCode::Cancelled)?;
        let reader = Arc::new(FileReader {
            data,
            ledger: ledger.clone(),
            owner: owner.into(),
        });
        session.commit(session.prepare_reader(reader)?)?;
        let url = session.root_url()?;
        Ok(Self {
            url,
            relay: bound.start(session)?,
        })
    }
    /// Background-only release before the same task probes its output, so it
    /// doesn't temporarily acquire a third playback-session slot.
    pub fn close(self) -> Result<(), ErrorCode> {
        self.relay.stop();
        let end = Instant::now() + Duration::from_secs(5);
        while !self.relay.is_quiescent() {
            if Instant::now() >= end {
                return Err(ErrorCode::Timeout);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}
