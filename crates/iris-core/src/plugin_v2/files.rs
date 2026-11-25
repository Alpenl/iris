//! Inert temporary artifacts and host-pinned local media hashing.
//! No guest-supplied path is accepted by this module.
mod artifact;
mod executor;
mod hashing;
pub mod identity;
pub use artifact::{Artifact, ArtifactWriter, TempRoot};
pub use executor::{FileExecutor, FileSetup};
pub use hashing::hash_media;
use iris_api::v2::ErrorCode;

pub(crate) fn random_name() -> Result<String, ErrorCode> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests;
