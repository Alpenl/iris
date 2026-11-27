//! Shared network, transport, DNS/DoH, loopback HTTP, and WebSocket/TCP framing.

#[doc(hidden)]
pub mod fuzz_net;
pub mod http_client;
pub mod loopback_http;
pub mod wire;
pub mod ws_tcp;
