//! Framing helpers; the host remains responsible for cryptographic computation.
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtrWindow {
    pub counter: [u8; 16],
    pub ciphertext_offset: u64,
    pub fetch_len: usize,
    pub discard: usize,
}

/// Big-endian AES-CTR byte seeking. Fetch the indicated ciphertext, decrypt it
/// with `counter`, and discard exactly `discard` leading plaintext bytes.
pub fn ctr_window(iv: [u8; 16], offset: u64, len: usize) -> Result<CtrWindow, ErrorCode> {
    let discard = if len == 0 { 0 } else { (offset % 16) as usize };
    let fetch_len = len.checked_add(discard).ok_or(ErrorCode::RangeInvalid)?;
    if fetch_len > CHUNK_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    offset
        .checked_add(len as u64)
        .ok_or(ErrorCode::RangeInvalid)?;
    let counter = u128::from_be_bytes(iv)
        .checked_add(u128::from(offset / 16))
        .ok_or(ErrorCode::RangeInvalid)?;
    if fetch_len > 0 {
        counter
            .checked_add(((fetch_len - 1) / 16) as u128)
            .ok_or(ErrorCode::RangeInvalid)?;
    }
    Ok(CtrWindow {
        counter: counter.to_be_bytes(),
        ciphertext_offset: offset - offset % 16,
        fetch_len,
        discard,
    })
}

pub fn pad_pkcs7(bytes: &[u8]) -> Result<Vec<u8>, ErrorCode> {
    let padding = 16 - bytes.len() % 16;
    let length = bytes
        .len()
        .checked_add(padding)
        .ok_or(ErrorCode::RangeInvalid)?;
    if length > CHUNK_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(bytes);
    out.resize(length, padding as u8);
    Ok(out)
}
/// Invalid padding clears the mutable plaintext buffer. This does not turn CBC
/// into authenticated encryption or define a network padding-oracle policy.
pub fn unpad_pkcs7(bytes: &mut Vec<u8>) -> Result<(), ErrorCode> {
    let pad = bytes.last().copied().unwrap_or(0) as usize;
    let valid = !bytes.is_empty()
        && bytes.len().is_multiple_of(16)
        && (1..=16).contains(&pad)
        && bytes[bytes.len() - pad.min(bytes.len())..]
            .iter()
            .all(|&b| usize::from(b) == pad);
    if !valid {
        bytes.fill(0);
        bytes.clear();
        return Err(ErrorCode::AuthFailed);
    }
    bytes.truncate(bytes.len() - pad);
    Ok(())
}

/// One sequence per key/nonce space. The caller must persist its position or use
/// a new key after restart; recreating it under the same key can reuse nonces.
pub struct NonceSequence {
    next: u128,
    exhausted: bool,
}
impl NonceSequence {
    pub fn new(first: [u8; 12]) -> Self {
        let mut expanded = [0; 16];
        expanded[4..].copy_from_slice(&first);
        Self {
            next: u128::from_be_bytes(expanded),
            exhausted: false,
        }
    }
    pub fn take(&mut self) -> Result<[u8; 12], ErrorCode> {
        if self.exhausted {
            return Err(ErrorCode::RangeInvalid);
        }
        let bytes = self.next.to_be_bytes();
        let mut result = [0; 12];
        result.copy_from_slice(&bytes[4..]);
        if self.next == (1u128 << 96) - 1 {
            self.exhausted = true;
        } else {
            self.next += 1;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ctr_windows_align_reads_and_fail_before_counter_wrap() {
        let window = ctr_window([0; 16], 31, 5).unwrap();
        assert_eq!(window.ciphertext_offset, 16);
        assert_eq!(window.fetch_len, 20);
        assert_eq!(window.discard, 15);
        assert_eq!(window.counter[15], 1);
        assert!(ctr_window([255; 16], 16, 1).is_err());
        assert!(ctr_window([255; 16], 15, 2).is_err());
        assert!(ctr_window([0; 16], u64::MAX, 1).is_err());
        assert!(ctr_window([0; 16], 1, CHUNK_LIMIT).is_err());
    }
    #[test]
    fn padding_handles_exact_blocks_and_erases_invalid_plaintext() {
        for n in 0..34 {
            let mut data = pad_pkcs7(&vec![42; n]).unwrap();
            assert_eq!(data.len() % 16, 0);
            unpad_pkcs7(&mut data).unwrap();
            assert_eq!(data, vec![42; n]);
        }
        let mut invalid = vec![0; 16];
        assert_eq!(unpad_pkcs7(&mut invalid), Err(ErrorCode::AuthFailed));
        assert!(invalid.is_empty());
        let mut invalid = vec![1; 16];
        invalid[15] = 2;
        assert!(unpad_pkcs7(&mut invalid).is_err());
    }
    #[test]
    fn nonce_sequence_exhausts_without_repeating() {
        let mut sequence = NonceSequence::new([255; 12]);
        assert_eq!(sequence.take().unwrap(), [255; 12]);
        assert_eq!(sequence.take(), Err(ErrorCode::RangeInvalid));
    }
}
