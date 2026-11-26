use super::{
    NetworkGrant, TransportKind,
    cookies::CookieJar,
    parse_consent_origin_url,
    raw::{self, RawRequest, RawState},
};
mod socket_tasks;
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        instance::{Blob, next_id},
        ledger::{Activity, Ledger, Reservation, Resource},
    },
    vfs::HttpCancel,
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode, Generation, Id, Role, task::Task};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct HttpHandle {
    instance: Id,
    generation: Generation,
    cancel: HttpCancel,
    busy: AtomicBool,
    state: Mutex<RawState>,
    _reservation: Reservation,
}
struct Busy(Arc<HttpHandle>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
struct Handles {
    instance: Option<Id>,
    generation: Option<Generation>,
    http: BTreeMap<u64, Arc<HttpHandle>>,
    sockets: BTreeMap<u64, socket_tasks::SocketHandle>,
}
struct Inner {
    grant: NetworkGrant,
    ledger: Ledger,
    cookies: Arc<CookieJar>,
    handles: Mutex<Handles>,
    stream_origins: BTreeSet<Id>,
    stopped: AtomicBool,
    activity: Activity,
}

/// Create one executor per Store. It rejects cross-instance reuse even when the
/// plugin identity is the same; stream origins are explicit context capabilities.
#[derive(Clone)]
pub struct NetworkExecutor(Arc<Inner>);
impl NetworkExecutor {
    pub fn new(
        grant: NetworkGrant,
        ledger: Ledger,
        stream_origins: BTreeSet<Id>,
    ) -> Result<Self, ErrorCode> {
        let cookies = Arc::new(CookieJar::new(grant.owner(), &ledger)?);
        Ok(Self(Arc::new(Inner {
            grant,
            ledger,
            cookies,
            stream_origins,
            stopped: AtomicBool::new(false),
            activity: Activity::default(),
            handles: Mutex::new(Handles {
                instance: None,
                generation: None,
                http: BTreeMap::new(),
                sockets: BTreeMap::new(),
            }),
        })))
    }

    fn bind(&self, context: &TaskContext) -> Result<(), ErrorCode> {
        if context.owner != self.0.grant.owner()
            || self.0.stopped.load(Ordering::Acquire)
            || context.cancel.is_cancelled()
        {
            return Err(ErrorCode::Cancelled);
        }
        let mut handles = self.0.handles.lock().unwrap_or_else(|e| e.into_inner());
        if handles.instance.is_some_and(|id| id != context.instance_id) {
            return Err(ErrorCode::Denied);
        }
        if handles.generation.is_some_and(|g| {
            g.media != context.generation.media || g.seek != context.generation.seek
        }) {
            return Err(ErrorCode::StaleHandle);
        }
        handles.instance = Some(context.instance_id);
        handles.generation.get_or_insert(context.generation);
        Ok(())
    }

    fn handle(&self, id: Id, context: &TaskContext) -> Result<(Arc<HttpHandle>, Busy), ErrorCode> {
        self.bind(context)?;
        let handles = self.0.handles.lock().unwrap_or_else(|e| e.into_inner());
        let handle = handles
            .http
            .get(&id.0)
            .cloned()
            .ok_or(ErrorCode::StaleHandle)?;
        if handle.instance != context.instance_id
            || handle.generation.media != context.generation.media
            || handle.generation.seek != context.generation.seek
        {
            return Err(ErrorCode::StaleHandle);
        }
        if handle.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        handle
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ErrorCode::BadState)?;
        let busy = Busy(handle.clone());
        // Cancelling a read/write also closes its underlying response socket.
        context
            .cancel
            .link(&handle.cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        Ok((handle, busy))
    }

    fn opened(&self, id: u64, value: serde_json::Value) -> IoOutcome {
        let weak = Arc::downgrade(&self.0);
        IoOutcome::new(value, None).on_discard(move || {
            if let Some(inner) = weak.upgrade() {
                // Until delivery is acknowledged, the guest cannot close this
                // handle. Cancellation or a dropped mailbox must reclaim it.
                let _ = Self(inner).close(id);
            }
        })
    }
}

impl Executor for NetworkExecutor {
    fn is_quiescent(&self) -> bool {
        self.0.activity.is_idle()
    }
    fn prepare(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        self.bind(&context)?;
        match task {
            Task::OriginLookup { origin } => {
                if context.role != Role::Control {
                    return Err(ErrorCode::Denied);
                }
                let (_, kind) = parse_consent_origin_url(&origin)?;
                let grant = self.0.grant.clone();
                let activity = self.0.activity.start();
                Ok(Operation::blocking(move || {
                    let _activity = activity;
                    if context.cancel.is_cancelled() {
                        return Err(ErrorCode::Cancelled);
                    }
                    let result = grant.lookup_origin(&origin, kind)?;
                    Ok(IoOutcome::new(
                        serde_json::to_value(result).map_err(|_| ErrorCode::Internal)?,
                        None,
                    ))
                }))
            }
            task @ (Task::WsConnect { .. }
            | Task::WsRead { .. }
            | Task::WsWrite { .. }
            | Task::TcpConnect { .. }
            | Task::TcpRead { .. }
            | Task::TcpWrite { .. }) => self.prepare_socket(task, blobs, context),
            Task::HttpStart {
                method,
                url,
                headers,
                body_handle,
                stream_body,
                follow_redirects,
                timeout_ms,
                origin_grant,
            } => {
                if context.role == Role::Stream
                    && origin_grant.is_some_and(|id| !self.0.stream_origins.contains(&id))
                {
                    return Err(ErrorCode::Denied);
                }
                if stream_body && body_handle.is_some() {
                    return Err(ErrorCode::InvalidArgument);
                }
                if method.is_empty() || method.len() > 32 || method != method.to_ascii_uppercase() {
                    return Err(ErrorCode::InvalidArgument);
                }
                raw::validate_headers(&headers)?;
                let (grant, url) = self.0.grant.request_normalized(
                    &url,
                    TransportKind::Http,
                    origin_grant,
                    self.0.activity.clone(),
                )?;
                grant.check_credentials(&url, &headers)?;
                let body = body_handle
                    .map(|id| blobs.get(&id.0).cloned().ok_or(ErrorCode::StaleHandle))
                    .transpose()?;
                let mut amounts = vec![
                    (Resource::Http, 1),
                    (Resource::Handle, 1),
                    (Resource::HostBytes, 2 << 20),
                ];
                if context.role == Role::Control {
                    amounts.push((Resource::ControlHttp, 1));
                }
                let reservation = self.0.ledger.reserve(self.0.grant.owner(), &amounts)?;
                let timeout = timeout_ms.unwrap_or(if context.role == Role::Stream {
                    30000
                } else {
                    60000
                });
                if timeout == 0 {
                    return Err(ErrorCode::InvalidArgument);
                }
                let timeout = Duration::from_millis(u64::from(timeout.min(
                    if context.role == Role::Stream {
                        30000
                    } else {
                        60000
                    },
                )));
                let cookies = grant.cookies().unwrap_or_else(|| self.0.cookies.clone());
                let request = RawRequest {
                    method,
                    url,
                    headers,
                    follow_redirects,
                    timeout,
                    cookies: Some(cookies),
                };
                let this = self.clone();
                Ok(Operation::blocking(move || {
                    let state = raw::open(
                        request,
                        body.as_ref().map(|b| b.bytes.as_slice()),
                        stream_body,
                        grant,
                        context.cancel.clone(),
                    )?;
                    let id = next_id()?;
                    let result = metadata(id, &state)?;
                    let handle = Arc::new(HttpHandle {
                        instance: context.instance_id,
                        generation: context.generation,
                        cancel: context.cancel.clone(),
                        busy: AtomicBool::new(false),
                        state: Mutex::new(state),
                        _reservation: reservation,
                    });
                    let mut handles = this.0.handles.lock().unwrap_or_else(|e| e.into_inner());
                    if this.0.stopped.load(Ordering::Acquire)
                        || context.cancel.is_cancelled()
                        || handles.generation.is_some_and(|g| {
                            g.media != context.generation.media || g.seek != context.generation.seek
                        })
                    {
                        return Err(ErrorCode::Cancelled);
                    }
                    handles.http.insert(id, handle);
                    drop(handles);
                    Ok(this.opened(id, result))
                }))
            }
            Task::HttpRead { handle, len } => {
                if len == 0 || len as usize > CHUNK_LIMIT {
                    return Err(ErrorCode::InvalidArgument);
                }
                let (handle, busy) = self.handle(handle, &context)?;
                let scratch = self.0.ledger.reserve(
                    self.0.grant.owner(),
                    &[(Resource::HostBytes, u64::from(len))],
                )?;
                Ok(Operation::blocking(move || {
                    let _scratch = scratch;
                    let mut bytes = vec![0; len as usize];
                    let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
                    let n = match state.read(&mut bytes, Instant::now() + Duration::from_secs(30)) {
                        Ok(n) => n,
                        Err(error) => {
                            *state = RawState::Closed;
                            return Err(error);
                        }
                    };
                    bytes.truncate(n);
                    let blob = Arc::new(Blob::new(&context.ledger, &context.owner, bytes)?);
                    let mut outcome = IoOutcome::new(serde_json::json!({"eof":n==0}), Some(blob));
                    outcome.pending = Some(Box::new(busy));
                    Ok(outcome)
                }))
            }
            Task::HttpWrite {
                handle,
                blob,
                finish,
            } => {
                let blob = blobs.get(&blob.0).cloned().ok_or(ErrorCode::StaleHandle)?;
                let id = handle.0;
                let (handle, busy) = self.handle(handle, &context)?;
                Ok(Operation::blocking(move || {
                    let mut state = handle.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(error) = state.write(&blob.bytes, finish) {
                        *state = RawState::Closed;
                        return Err(error);
                    }
                    let mut value = metadata(id, &state)?;
                    value["written"] = serde_json::json!(Id(blob.bytes.len() as u64));
                    let mut outcome = IoOutcome::new(value, None);
                    outcome.pending = Some(Box::new(busy));
                    Ok(outcome)
                }))
            }
            _ => Err(ErrorCode::Unsupported),
        }
    }

    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        let socket = self
            .0
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sockets
            .remove(&handle);
        if let Some(socket) = socket {
            socket.socket.close();
            return Ok(());
        }
        let handle = self
            .0
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .http
            .remove(&handle)
            .ok_or(ErrorCode::StaleHandle)?;
        handle.cancel.cancel();
        Ok(())
    }
    fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        let sockets = std::mem::take(
            &mut self
                .0
                .handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sockets,
        );
        for (_, handle) in sockets {
            handle.socket.close();
        }
        let handles = std::mem::take(
            &mut self
                .0
                .handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .http,
        );
        for (_, handle) in handles {
            handle.cancel.cancel();
        }
    }
    fn generation_changed(&self, generation: Generation) {
        let (sockets, old) = {
            let mut handles = self.0.handles.lock().unwrap_or_else(|e| e.into_inner());
            handles.generation = Some(generation);
            (
                std::mem::take(&mut handles.sockets),
                std::mem::take(&mut handles.http),
            )
        };
        for (_, handle) in sockets {
            handle.socket.close();
        }
        for (_, handle) in old {
            handle.cancel.cancel();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let handles = self.handles.get_mut().unwrap_or_else(|e| e.into_inner());
        for handle in handles.http.values() {
            handle.cancel.cancel();
        }
        for handle in handles.sockets.values() {
            handle.socket.close();
        }
    }
}

fn metadata(id: u64, state: &RawState) -> Result<serde_json::Value, ErrorCode> {
    let value = match state {
        RawState::Upload(_) => Ok(serde_json::json!({"state":"upload","stream_handle":Id(id)})),
        RawState::Response(response) => Ok(
            serde_json::json!({"state":"response","stream_handle":Id(id),"status":response.status,"url":response.url.as_str(),"headers":response.headers}),
        ),
        RawState::Closed => Err(ErrorCode::BadState),
    }?;
    crate::bounded_json::encode(&value, iris_api::v2::MESSAGE_LIMIT - 1024)
        .map_err(crate::plugin_v2::json_error)?;
    Ok(value)
}
