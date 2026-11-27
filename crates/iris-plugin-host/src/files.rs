//! Inert temporary artifacts and host-pinned local media hashing.
//! No guest-supplied path is accepted by this module.
mod artifact;
mod executor;
mod hashing;
pub(crate) mod identity;
pub(crate) use artifact::{Artifact, ArtifactCleaner, ArtifactWriter, CleanerQueue, TempRoot};
pub(crate) use executor::{FileExecutor, FileSetup};
pub(crate) use hashing::hash_media;

#[cfg(test)]
mod tests;
