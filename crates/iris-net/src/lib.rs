//! Shared network, transport, DNS/DoH, loopback relay, WebSocket/TCP framing,
//! and managed concurrency primitives for Iris.

pub mod concurrency;
pub mod http_client;
pub mod loopback_http;
pub mod wire;
pub mod ws_tcp;
