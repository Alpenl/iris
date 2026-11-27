use super::reader::{Reader, check_read};
use crate::{
    plugin_v2::{
        actor::{Actor, ReadResult, Spawn},
        instance::Blob,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{Capability, ErrorCode, Generation, Id, Role, WireU64, stream::StreamContext};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct State {
    actor: Actor,
    generation: Generation,
    expires: Option<WireU64>,
}
pub struct StreamStore {
    state: Mutex<State>,
    session: Id,
    origins: BTreeSet<Id>,
    transports: BTreeSet<Capability>,
}
impl StreamStore {
    pub fn spawn(
        mut spawn: Spawn,
        session: Id,
        context: &StreamContext,
        blob: Arc<Blob>,
    ) -> Result<Self, ErrorCode> {
        validate_context(context)?;
        let transports: BTreeSet<_> = context.transports.iter().copied().collect();
        if spawn.role != Role::Stream || !transports.is_subset(&spawn.grants) {
            return Err(ErrorCode::Denied);
        }
        spawn.grants = transports.clone();
        spawn.generation.context = context.version;
        let generation = spawn.generation;
        let actor = Actor::spawn(spawn)?;
        actor.send(
            crate::bounded_json::encode(
                &serde_json::json!({
                    "event":"init", "protocol":2, "abi":2, "host_version":env!("CARGO_PKG_VERSION"),
                    "role":"stream", "generation":generation, "capabilities":transports, "features":["stream_read","stream_context_changed"],
                    "snapshot":null
                }),
                iris_api::v2::MESSAGE_LIMIT,
            )
            .map_err(crate::plugin_v2::json_error)?,
        )?;
        actor.send_context(
            serde_json::json!({"event":"stream_init","session":session,"generation":generation}),
            blob,
        )?;
        Ok(Self {
            state: Mutex::new(State {
                actor,
                generation,
                expires: context.expires_unix_ms,
            }),
            session,
            origins: context.origin_grants.iter().copied().collect(),
            transports,
        })
    }
    pub fn refresh(
        &self,
        expected: Id,
        context: &StreamContext,
        blob: Arc<Blob>,
    ) -> Result<(), ErrorCode> {
        validate_context(context)?;
        // Broader authority requires a new explicit stream admission. Refresh
        // may rotate bytes/keys/URLs inside this Store's original authority.
        if context
            .origin_grants
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            != self.origins
            || context.transports.iter().copied().collect::<BTreeSet<_>>() != self.transports
        {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation.context != expected {
            return Err(ErrorCode::StaleHandle);
        }
        let generation = Generation {
            context: context.version,
            ..state.generation
        };
        state
            .actor
            .refresh_context(self.session, state.generation, generation, blob)?;
        state.generation = generation;
        state.expires = context.expires_unix_ms;
        Ok(())
    }
    pub fn advance_seek(&self, seek: Id) -> Result<(), ErrorCode> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let generation = Generation {
            seek,
            ..state.generation
        };
        state.actor.advance(generation)?;
        state.generation = generation;
        Ok(())
    }
    pub fn generation(&self) -> Generation {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }
    pub fn stop(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actor
            .stop();
    }
    pub fn is_quiescent(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actor
            .is_quiescent()
    }
    pub fn reader(
        self: &Arc<Self>,
        resource: String,
        size: Option<u64>,
    ) -> Result<Arc<dyn Reader>, ErrorCode> {
        super::manifest::logical(&resource)?;
        Ok(Arc::new(GuestReader {
            store: self.clone(),
            resource,
            size,
        }))
    }
}
impl Drop for StreamStore {
    fn drop(&mut self) {
        self.stop();
    }
}
struct GuestReader {
    store: Arc<StreamStore>,
    resource: String,
    size: Option<u64>,
}
impl Reader for GuestReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        check_expiry(
            self.store
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .expires,
        )?;
        Ok(self.size)
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        let (ticket, expires) = {
            let state = self.store.state.lock().unwrap_or_else(|e| e.into_inner());
            check_expiry(state.expires)?;
            let ticket = state
                .actor
                .read(self.resource.clone(), offset, len, self.size)?;
            (ticket, state.expires)
        };
        let result = ticket.receive_cancellable(cancel, Duration::from_secs(30))?;
        check_expiry(expires)?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        Ok(result)
    }
}
fn check_expiry(expires: Option<WireU64>) -> Result<(), ErrorCode> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ErrorCode::Internal)?
        .as_millis();
    if expires.is_some_and(|n| u128::from(n.0) <= now) {
        return Err(ErrorCode::StaleHandle);
    }
    Ok(())
}
fn validate_context(context: &StreamContext) -> Result<(), ErrorCode> {
    if context.version.0 == 0
        || context.origin_grants.len() > 16
        || context.transports.len() > 3
        || context.transports.iter().any(|c| {
            !matches!(
                c,
                Capability::NetHttp | Capability::NetWs | Capability::NetTcp
            )
        })
        || context
            .origin_grants
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != context.origin_grants.len()
        || context
            .transports
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != context.transports.len()
    {
        return Err(ErrorCode::InvalidArgument);
    }
    check_expiry(context.expires_unix_ms)
}
