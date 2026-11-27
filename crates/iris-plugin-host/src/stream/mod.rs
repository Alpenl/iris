//! Session-scoped resources and strictly generated adaptive manifests.
pub(crate) mod guest;
pub(crate) mod manifest;
pub(crate) mod prepare;
pub(crate) mod range;
pub(crate) mod reader;
pub(crate) mod relay;

pub(crate) mod remote;
pub(crate) mod remote_manifest;
pub(crate) mod session;
#[cfg(test)]
mod tests;
