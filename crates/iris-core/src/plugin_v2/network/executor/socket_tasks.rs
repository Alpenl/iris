use super::super::{
    TransportKind, normalize_url, raw,
    socket::{Action, Socket},
};
use super::NetworkExecutor;
use crate::plugin_v2::{
    actor::{IoOutcome, Operation, TaskContext},
    instance::{Blob, next_id},
    ledger::Resource,
};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode, Generation, Id, Role, task::Task};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

pub(super) struct SocketHandle {
    pub instance: Id,
    pub generation: Generation,
    pub socket: Socket,
}
impl NetworkExecutor {
    pub(super) fn prepare_socket(
        &self,
        task: Task,
        blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        match task {
            Task::WsConnect {
                url,
                headers,
                origin_grant,
            } => {
                raw::validate_headers(&headers)?;
                if headers
                    .iter()
                    .any(|(n, _)| n.to_ascii_lowercase().starts_with("sec-websocket-"))
                {
                    return Err(ErrorCode::Denied);
                }
                self.connect_socket(
                    url,
                    headers,
                    TransportKind::WebSocket,
                    origin_grant,
                    context,
                )
            }
            Task::TcpConnect {
                host,
                port,
                tls,
                origin_grant,
            } => {
                if host.is_empty() || host.contains(['/', '@', '?', '#', '\\']) || port == 0 {
                    return Err(ErrorCode::InvalidArgument);
                }
                let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
                    format!("[{host}]")
                } else {
                    host
                };
                let url = format!(
                    "{}://{}:{}/",
                    if tls { "https" } else { "http" },
                    host,
                    port
                );
                // URL parsing must not silently reinterpret a host supplied with its own port.
                let parsed = normalize_url(&url, TransportKind::Tcp { tls })?;
                if parsed.port_or_known_default() != Some(port) {
                    return Err(ErrorCode::InvalidArgument);
                }
                self.connect_socket(
                    url,
                    Vec::new(),
                    TransportKind::Tcp { tls },
                    origin_grant,
                    context,
                )
            }
            Task::WsRead { handle } => self
                .socket(handle, &context, true)?
                .operation(Action::Read(CHUNK_LIMIT as u32), context),
            Task::TcpRead { handle, len } => self
                .socket(handle, &context, false)?
                .operation(Action::Read(len), context),
            Task::WsWrite { handle, blob, text } => {
                let blob = blobs.get(&blob.0).cloned().ok_or(ErrorCode::StaleHandle)?;
                self.socket(handle, &context, true)?
                    .operation(Action::Write { blob, text }, context)
            }
            Task::TcpWrite { handle, blob } => {
                let blob = blobs.get(&blob.0).cloned().ok_or(ErrorCode::StaleHandle)?;
                self.socket(handle, &context, false)?
                    .operation(Action::Write { blob, text: false }, context)
            }
            _ => Err(ErrorCode::Unsupported),
        }
    }
    fn socket(&self, id: Id, context: &TaskContext, ws: bool) -> Result<Socket, ErrorCode> {
        let handles = self.0.handles.lock().unwrap_or_else(|e| e.into_inner());
        let handle = handles.sockets.get(&id.0).ok_or(ErrorCode::StaleHandle)?;
        if handle.instance != context.instance_id
            || handle.generation.media != context.generation.media
            || handle.generation.seek != context.generation.seek
        {
            return Err(ErrorCode::StaleHandle);
        }
        if handle.socket.is_ws() != ws {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(handle.socket.clone())
    }
    fn connect_socket(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        kind: TransportKind,
        origin_grant: Option<Id>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if context.role == Role::Stream
            && origin_grant.is_some_and(|id| !self.0.stream_origins.contains(&id))
        {
            return Err(ErrorCode::Denied);
        }
        let grant =
            self.0
                .grant
                .request_tracked(&url, kind, origin_grant, self.0.activity.clone())?;
        let url = normalize_url(&url, kind)?;
        grant.check_credentials(&url, &headers)?;
        let ws = kind == TransportKind::WebSocket;
        let reservation = self.0.ledger.reserve(
            self.0.grant.owner(),
            &[
                (
                    if ws {
                        Resource::WebSocket
                    } else {
                        Resource::Tcp
                    },
                    1,
                ),
                (Resource::Handle, 1),
                (Resource::HostBytes, if ws { 8 << 20 } else { 2 << 20 }),
            ],
        )?;
        let this = self.clone();
        Ok(Operation::blocking(move || {
            let socket = Socket::connect(
                &url,
                &headers,
                ws,
                grant,
                context.cancel.clone(),
                reservation,
            )?;
            let id = next_id()?;
            let mut handles = this.0.handles.lock().unwrap_or_else(|e| e.into_inner());
            if this.0.stopped.load(Ordering::Acquire)
                || context.cancel.is_cancelled()
                || handles.generation.is_some_and(|g| {
                    g.media != context.generation.media || g.seek != context.generation.seek
                })
            {
                socket.close();
                return Err(ErrorCode::Cancelled);
            }
            handles.sockets.insert(
                id,
                SocketHandle {
                    instance: context.instance_id,
                    generation: context.generation,
                    socket,
                },
            );
            Ok(IoOutcome::new(
                serde_json::json!({"stream_handle":Id(id)}),
                None,
            ))
        }))
    }
}
