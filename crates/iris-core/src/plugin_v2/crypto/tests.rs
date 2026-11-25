use super::*;
use iris_api::v2::Id;
fn hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn hashes_and_hmac_match_standard_vectors() {
    for (algorithm, expected) in [
        (Hash::Md5, "900150983cd24fb0d6963f7d28e17f72"),
        (Hash::Sha1, "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (
            Hash::Sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            Hash::Blake3,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
        ),
    ] {
        assert_eq!(
            compute(
                &CryptoSpec::Hash { algorithm },
                b"abc",
                32,
                |_| unreachable!()
            )
            .unwrap(),
            hex(expected)
        );
    }
    assert_eq!(
        compute(
            &CryptoSpec::HmacSha256 { key: Id(1) },
            b"Hi There",
            32,
            |_| Ok(vec![0x0b; 20])
        )
        .unwrap(),
        hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
    );
}

#[test]
fn nist_ctr_cbc_and_counter_boundary() {
    let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
    let iv = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
    let input = hex("6bc1bee22e409f96e93d7e117393172a");
    assert_eq!(
        ctr::<aes::Aes128>(&key, &iv, &input).unwrap(),
        hex("874d6191b620e3261bef6864990db6ce")
    );
    assert!(ctr::<aes::Aes128>(&key, &[255; 16], &[0; 16]).is_ok());
    assert_eq!(
        ctr::<aes::Aes128>(&key, &[255; 16], &[0; 17]).unwrap_err(),
        ErrorCode::RangeInvalid
    );
    let spec = CryptoSpec::Encrypt {
        algorithm: Cipher::Aes128Cbc,
        key: Id(1),
        nonce: Id(2),
        aad: None,
    };
    let iv = hex("000102030405060708090a0b0c0d0e0f");
    assert_eq!(
        compute(&spec, &input, 16, |id| Ok(if id == 1 {
            key.clone()
        } else {
            iv.clone()
        }))
        .unwrap(),
        hex("7649abac8119b246cee98e9b12e9197d")
    );
    assert!(compute(&spec, &[0; 17], 17, |_| Ok(vec![0; 16])).is_err());
}

#[test]
fn gcm_vector_and_auth_failure_never_return_plaintext() {
    let spec = CryptoSpec::Encrypt {
        algorithm: Cipher::Aes256Gcm,
        key: Id(1),
        nonce: Id(2),
        aad: None,
    };
    let blobs = |id| Ok(if id == 1 { vec![0; 32] } else { vec![0; 12] });
    let ciphertext = compute(&spec, &[0; 16], 32, blobs).unwrap();
    assert_eq!(
        ciphertext,
        hex("cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919")
    );
    let spec = CryptoSpec::Decrypt {
        algorithm: Cipher::Aes256Gcm,
        key: Id(1),
        nonce: Id(2),
        aad: None,
    };
    assert_eq!(compute(&spec, &ciphertext, 16, blobs).unwrap(), vec![0; 16]);
    let mut bad = ciphertext;
    bad[31] ^= 1;
    assert_eq!(
        compute(&spec, &bad, 16, blobs).unwrap_err(),
        ErrorCode::AuthFailed
    );
}

#[test]
fn rfc8439_chacha20_poly1305_authenticated_vector() {
    // RFC 8439 §2.8.2: https://www.rfc-editor.org/rfc/rfc8439.html#section-2.8.2
    let ciphertext = hex(concat!(
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6",
        "3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36",
        "92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc",
        "3ff4def08e4b7a9de576d26586cec64b61161ae10b594f09e26a7e902ecbd0600691"
    ));
    let blobs = |id| {
        Ok(match id {
            1 => (0x80..=0x9f).collect(),
            2 => hex("070000004041424344454647"),
            3 => hex("50515253c0c1c2c3c4c5c6c7"),
            _ => unreachable!(),
        })
    };
    let spec = CryptoSpec::Decrypt {
        algorithm: Cipher::ChaCha20Poly1305,
        key: Id(1),
        nonce: Id(2),
        aad: Some(Id(3)),
    };
    let plain = compute(&spec, &ciphertext, 114, blobs).unwrap();
    assert_eq!(plain.len(), 114);
    assert!(plain.starts_with(b"Ladies and Gentlemen"));
    assert!(plain.ends_with(b"sunscreen would be it."));
    let encrypt = CryptoSpec::Encrypt {
        algorithm: Cipher::ChaCha20Poly1305,
        key: Id(1),
        nonce: Id(2),
        aad: Some(Id(3)),
    };
    assert_eq!(compute(&encrypt, &plain, 130, blobs).unwrap(), ciphertext);
    let mut corrupted = ciphertext;
    corrupted[0] ^= 1;
    assert_eq!(
        compute(&spec, &corrupted, 114, blobs),
        Err(ErrorCode::AuthFailed)
    );
}

#[test]
fn aes256_ctr_vector_and_sdk_unaligned_windows() {
    // NIST SP 800-38A F.5.5, first block.
    let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let iv = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
    assert_eq!(
        ctr::<aes::Aes256>(&key, &iv, &hex("6bc1bee22e409f96e93d7e117393172a")).unwrap(),
        hex("601ec313775789a5b7a7f504bbf3d228")
    );
    let input: Vec<u8> = (0..256).map(|n| n as u8).collect();
    let cipher = ctr::<aes::Aes256>(&key, &iv, &input).unwrap();
    let iv_arr: [u8; 16] = iv.clone().try_into().unwrap();
    for offset in 0u64..64 {
        for len in 1usize..33 {
            let discard = (offset % 16) as usize;
            let start = (offset - offset % 16) as usize;
            let fetch_len = len + discard;
            let counter = (u128::from_be_bytes(iv_arr) + u128::from(offset / 16)).to_be_bytes();
            let decoded =
                ctr::<aes::Aes256>(&key, &counter, &cipher[start..start + fetch_len]).unwrap();
            assert_eq!(
                &decoded[discard..],
                &input[offset as usize..offset as usize + len]
            );
        }
    }
}
