use super::{Converted, convert};
use crate::{
    plugin_v2::stream::{
        remote::RemoteFactory,
        session::{Refresh, StreamSession},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{ErrorCode, stream::RemoteResource};
use std::{
    sync::{Arc, Mutex, TryLockError, Weak},
    time::{Duration, Instant},
};
struct State {
    next: Instant,
    error: Option<ErrorCode>,
}
struct Live {
    session: Weak<StreamSession>,
    factory: RemoteFactory,
    source: RemoteResource,
    state: Mutex<State>,
}
impl Refresh for Live {
    fn refresh(&self, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Poisoned(_)) => return Err(ErrorCode::Internal),
        };
        if Instant::now() < state.next {
            return state.error.map_or(Ok(()), Err);
        }
        let session = self.session.upgrade().ok_or(ErrorCode::Cancelled)?;
        let result = (|| {
            let document = convert(&self.factory, &self.source, cancel)?;
            let prepared =
                session.prepare_manifest(session.revision(), document.format, &document.bytes)?;
            session.commit(prepared)?;
            Ok::<_, ErrorCode>(document)
        })();
        match result {
            Ok(document) => {
                state.next = Instant::now()
                    + if document.live {
                        document.refresh_after
                    } else {
                        Duration::from_secs(86400 * 365)
                    };
                state.error = None;
                Ok(())
            }
            Err(error) => {
                if error != ErrorCode::Cancelled {
                    state.next = Instant::now() + Duration::from_secs(1);
                    state.error = Some(error);
                }
                Err(error)
            }
        }
    }
}
pub fn attach(
    session: &Arc<StreamSession>,
    source: RemoteResource,
    document: &Converted,
) -> Result<(), ErrorCode> {
    if !document.live {
        return Ok(());
    }
    session.set_refresh(Arc::new(Live {
        session: Arc::downgrade(session),
        factory: session.factory.clone(),
        source,
        state: Mutex::new(State {
            next: Instant::now() + document.refresh_after,
            error: None,
        }),
    }))
}
