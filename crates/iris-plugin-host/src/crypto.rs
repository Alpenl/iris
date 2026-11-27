//! Bounded, synchronous cryptographic primitives. Authenticated decryption never
//! publishes its scratch buffer before authentication succeeds.
use aes::cipher::{
    BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit, block_padding::NoPadding,
};
use iris_api::v2::{
    CHUNK_LIMIT, ErrorCode,
    crypto::{Cipher, CryptoSpec, Hash},
};
use md5::Digest;
use ring::{
    aead, digest, hmac,
    rand::{SecureRandom, SystemRandom},
};

pub(crate) fn compute(
    spec: &CryptoSpec,
    input: &[u8],
    capacity: usize,
    mut blob: impl FnMut(u64) -> Result<Vec<u8>, ErrorCode>,
) -> Result<Vec<u8>, ErrorCode> {
    if input.len() > CHUNK_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    let required = match spec {
        CryptoSpec::Hash { algorithm } => match algorithm {
            Hash::Md5 => 16,
            Hash::Sha1 => 20,
            Hash::Sha256 | Hash::Blake3 => 32,
        },
        CryptoSpec::HmacSha256 { .. } => 32,
        CryptoSpec::Encrypt {
            algorithm: Cipher::Aes256Gcm | Cipher::ChaCha20Poly1305,
            ..
        } => input.len() + 16,
        CryptoSpec::Decrypt {
            algorithm: Cipher::Aes256Gcm | Cipher::ChaCha20Poly1305,
            ..
        } => input
            .len()
            .checked_sub(16)
            .ok_or(ErrorCode::InvalidArgument)?,
        _ => input.len(),
    };
    if capacity < required {
        return Err(ErrorCode::RangeInvalid);
    }
    match spec {
        CryptoSpec::Hash { algorithm } => Ok(match algorithm {
            Hash::Md5 => md5::Md5::digest(input).to_vec(),
            Hash::Sha1 => sha1::Sha1::digest(input).to_vec(),
            Hash::Sha256 => digest::digest(&digest::SHA256, input).as_ref().to_vec(),
            Hash::Blake3 => blake3::hash(input).as_bytes().to_vec(),
        }),
        CryptoSpec::HmacSha256 { key } => {
            let key = blob(key.0)?;
            if key.len() > 65536 {
                return Err(ErrorCode::QuotaExceeded);
            }
            Ok(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), input)
                .as_ref()
                .to_vec())
        }
        CryptoSpec::Encrypt {
            algorithm,
            key,
            nonce,
            aad,
        }
        | CryptoSpec::Decrypt {
            algorithm,
            key,
            nonce,
            aad,
        } => {
            let encrypt = matches!(spec, CryptoSpec::Encrypt { .. });
            let key = blob(key.0)?;
            let iv = blob(nonce.0)?;
            let aad = aad.map(|id| blob(id.0)).transpose()?.unwrap_or_default();
            if aad.len() > CHUNK_LIMIT {
                return Err(ErrorCode::QuotaExceeded);
            }
            match algorithm {
                Cipher::Aes128Ctr | Cipher::Aes256Ctr => {
                    if !aad.is_empty() {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    match algorithm {
                        Cipher::Aes128Ctr => ctr::<aes::Aes128>(&key, &iv, input),
                        _ => ctr::<aes::Aes256>(&key, &iv, input),
                    }
                }
                Cipher::Aes128Cbc => {
                    if !aad.is_empty() || !input.len().is_multiple_of(16) {
                        return Err(ErrorCode::InvalidArgument);
                    }
                    if encrypt {
                        let c = cbc::Encryptor::<aes::Aes128>::new_from_slices(&key, &iv)
                            .map_err(|_| ErrorCode::InvalidArgument)?;
                        Ok(c.encrypt_padded_vec_mut::<NoPadding>(input))
                    } else {
                        let c = cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, &iv)
                            .map_err(|_| ErrorCode::InvalidArgument)?;
                        c.decrypt_padded_vec_mut::<NoPadding>(input)
                            .map_err(|_| ErrorCode::InvalidArgument)
                    }
                }
                Cipher::Aes256Gcm | Cipher::ChaCha20Poly1305 => {
                    let algorithm = if matches!(algorithm, Cipher::Aes256Gcm) {
                        &aead::AES_256_GCM
                    } else {
                        &aead::CHACHA20_POLY1305
                    };
                    let key = aead::LessSafeKey::new(
                        aead::UnboundKey::new(algorithm, &key)
                            .map_err(|_| ErrorCode::InvalidArgument)?,
                    );
                    let nonce = aead::Nonce::try_assume_unique_for_key(&iv)
                        .map_err(|_| ErrorCode::InvalidArgument)?;
                    let mut output = input.to_vec();
                    if encrypt {
                        key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut output)
                            .map_err(|_| ErrorCode::Internal)?;
                    } else {
                        match key.open_in_place(nonce, aead::Aad::from(aad), &mut output) {
                            Ok(plain) => {
                                let n = plain.len();
                                output.truncate(n);
                            }
                            Err(_) => {
                                output.fill(0);
                                return Err(ErrorCode::AuthFailed);
                            }
                        }
                    }
                    Ok(output)
                }
            }
        }
    }
}

fn ctr<C>(key: &[u8], iv: &[u8], input: &[u8]) -> Result<Vec<u8>, ErrorCode>
where
    C: BlockEncrypt + KeyInit + aes::cipher::BlockSizeUser<BlockSize = aes::cipher::consts::U16>,
{
    let start = u128::from_be_bytes(iv.try_into().map_err(|_| ErrorCode::InvalidArgument)?);
    if !input.is_empty()
        && start
            .checked_add(((input.len() - 1) / 16) as u128)
            .is_none()
    {
        return Err(ErrorCode::RangeInvalid);
    }
    let cipher = C::new_from_slice(key).map_err(|_| ErrorCode::InvalidArgument)?;
    let mut output = input.to_vec();
    for (i, block) in output.chunks_mut(16).enumerate() {
        let mut keystream = aes::cipher::Block::<C>::from((start + i as u128).to_be_bytes());
        cipher.encrypt_block(&mut keystream);
        for (byte, mask) in block.iter_mut().zip(keystream) {
            *byte ^= mask;
        }
    }
    Ok(output)
}

pub(crate) fn random_identity() -> Result<String, ErrorCode> {
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn valid_identity(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests;
