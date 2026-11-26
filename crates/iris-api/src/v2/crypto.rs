//! Bounded host-executed cryptographic operation specs (`iris_v2.host_crypto`).
//!
//! Intentionally retained in the ABI v2 contract so low-level or non-Rust Wasm
//! guests can delegate digest, HMAC, and AEAD/cipher work to the host without
//! shipping guest-side crypto implementations, even though high-level framing
//! helpers were omitted from `iris-plugin-sdk`.

use super::Id;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum CryptoSpec {
    Hash {
        algorithm: Hash,
    },
    HmacSha256 {
        key: Id,
    },
    Encrypt {
        algorithm: Cipher,
        key: Id,
        nonce: Id,
        aad: Option<Id>,
    },
    Decrypt {
        algorithm: Cipher,
        key: Id,
        nonce: Id,
        aad: Option<Id>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hash {
    Md5,
    Sha1,
    Sha256,
    Blake3,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cipher {
    Aes128Ctr,
    Aes256Ctr,
    Aes128Cbc,
    Aes256Gcm,
    ChaCha20Poly1305,
}
