use super::{Converted, convert};
use crate::stream::{
    remote::RemoteFactory,
    session::{Refresh, StreamSession},
};
use iris_api::v2::{ErrorCode, stream::RemoteResource};
use iris_net::http_client::HttpCancel;
use std::{
    sync::{Arc, Condvar, Mutex, Weak},
    time::{Duration, Instant},
};
struct State {
    next: Instant,
    error: Option<ErrorCode>,
    refreshing: bool,
}
struct Live {
    session: Weak<StreamSession>,
    factory: RemoteFactory,
    source: RemoteResource,
    state: Arc<Mutex<State>>,
    changed: Arc<Condvar>,
}
struct RefreshGuard<'a> {
    live: &'a Live,
    armed: bool,
}
impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.live.state.lock().unwrap_or_else(|e| e.into_inner());
            state.refreshing = false;
            self.live.changed.notify_all();
        }
    }
}
impl Refresh for Live {
    fn refresh(&self, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let state_arc = Arc::clone(&self.state);
        let changed_arc = Arc::clone(&self.changed);
        let _cancel_reg = cancel
            .register_callback(Arc::new(move || {
                let _guard = state_arc.lock().unwrap_or_else(|e| e.into_inner());
                changed_arc.notify_all();
            }))
            .map_err(|_| ErrorCode::Cancelled)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.refreshing {
            if cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if Instant::now() < state.next {
            return state.error.map_or(Ok(()), Err);
        }
        state.refreshing = true;
        drop(state);

        let mut guard = RefreshGuard {
            live: self,
            armed: true,
        };
        let session = self.session.upgrade().ok_or(ErrorCode::Cancelled)?;
        let result = (|| {
            let document = convert(&self.factory, &self.source, cancel)?;
            let prepared =
                session.prepare_manifest(session.revision(), document.format, &document.bytes)?;
            session.commit(prepared)?;
            Ok::<_, ErrorCode>(document)
        })();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.refreshing = false;
        guard.armed = false;
        let outcome = match result {
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
        };
        self.changed.notify_all();
        outcome
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
        state: Arc::new(Mutex::new(State {
            next: Instant::now() + document.refresh_after,
            error: None,
            refreshing: false,
        })),
        changed: Arc::new(Condvar::new()),
    }))
}
