//! Shared managed runtime (`rt`: cancellation, deadlines, thread registry and budgets)
//! and domain-free networking (`net`: HTTP/1.1 + rustls, DNS/DoH, loopback HTTP,
//! WebSocket/TCP framing, and parser fuzz entry points) for Iris.

pub mod net;
pub mod rt;
