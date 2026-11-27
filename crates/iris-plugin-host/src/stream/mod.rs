//! Session-scoped resources and strictly generated adaptive manifests.
pub mod guest;
pub mod manifest;
pub mod prepare;
pub mod range;
pub mod reader;
pub mod relay;

pub mod remote;
pub mod remote_manifest;
pub mod session;
#[cfg(test)]
mod tests;
