//! Inert temporary artifacts and host-pinned local media hashing.
//! No guest-supplied path is accepted by this module.
mod artifact;
mod executor;
mod hashing;
pub mod identity;
pub use artifact::{Artifact, ArtifactWriter, TempRoot};
pub use executor::{FileExecutor, FileSetup};
pub use hashing::hash_media;

#[cfg(test)]
mod tests;
