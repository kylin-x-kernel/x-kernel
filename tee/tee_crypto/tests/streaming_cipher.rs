// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::streaming_cipher::*;

#[test]
fn test_aes128_cbc_streaming_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Cbc,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"Hello, ").unwrap();
    ct.extend(enc.update(b"world!").unwrap());
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Cbc,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"Hello, world!");
}

#[test]
fn test_aes256_cbc_streaming_roundtrip() {
    let key = [0xABu8; 32];
    let iv = [0x00u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes256Cbc,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"test data").unwrap();
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes256Cbc,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"test data");
}

#[test]
fn test_aes128_ecb_streaming_roundtrip() {
    let key = [0x42u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ecb,
        &key,
        &[],
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"ECB test!").unwrap();
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ecb,
        &key,
        &[],
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"ECB test!");
}

#[test]
fn test_sm4_cbc_streaming_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Cbc,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"SM4 CBC streaming").unwrap();
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Cbc,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"SM4 CBC streaming");
}

#[test]
fn test_sm4_ecb_nopad_streaming_roundtrip() {
    let key = [0x42u8; 16];
    let plaintext = b"abcdefghabcdefgh1234567890987654";

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Ecb,
        &key,
        &[],
        Direction::Encrypt,
        PaddingMode::None,
    )
    .unwrap();
    assert_eq!(enc.max_update_output_len(16), 16);
    let mut ct = enc.update(&plaintext[..16]).unwrap();
    assert_eq!(enc.max_update_output_len(16), 16);
    ct.extend(enc.update(&plaintext[16..]).unwrap());
    assert_eq!(enc.max_update_output_len(0), 0);
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Ecb,
        &key,
        &[],
        Direction::Decrypt,
        PaddingMode::None,
    )
    .unwrap();
    assert_eq!(dec.max_update_output_len(16), 16);
    let mut pt = dec.update(&ct[..16]).unwrap();
    assert_eq!(dec.max_update_output_len(16), 16);
    pt.extend(dec.update(&ct[16..]).unwrap());
    assert_eq!(dec.max_update_output_len(0), 0);
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, plaintext);
}

#[test]
fn test_aes192_gcm_streaming_empty_tag() {
    let key = [0u8; 24];
    let nonce = [0u8; 12];
    let tag_expect = [
        0xcd, 0x33, 0xb2, 0x8a, 0xc7, 0x73, 0xf7, 0x4b, 0xa0, 0x0e, 0xd1, 0xf3, 0x12, 0x57, 0x24,
        0x35,
    ];

    let mut enc = StreamingCipherCtx::new_aead(
        StreamingCipherAlgo::Aes192Gcm,
        &key,
        &nonce,
        Direction::Encrypt,
        16,
    )
    .unwrap();
    let (tail, tag) = enc.encrypt_final_with_input(None).unwrap();
    assert!(tail.is_empty());
    assert_eq!(tag, tag_expect);
}

#[test]
fn test_sm4_gcm_final_input_returns_ciphertext() {
    let key = [0x42u8; 16];
    let nonce = [0x24u8; 12];
    let aad = b"aad";
    let plaintext = [0x5au8; 64];

    let mut enc = StreamingCipherCtx::new_aead(
        StreamingCipherAlgo::Sm4Gcm,
        &key,
        &nonce,
        Direction::Encrypt,
        16,
    )
    .unwrap();
    enc.update_aad(aad);
    let mut ciphertext = enc.update(&plaintext[..32]).unwrap();
    let (tail, tag) = enc
        .encrypt_final_with_input(Some(&plaintext[32..]))
        .unwrap();
    ciphertext.extend(tail);
    assert_eq!(ciphertext.len(), plaintext.len());

    let mut dec = StreamingCipherCtx::new_aead(
        StreamingCipherAlgo::Sm4Gcm,
        &key,
        &nonce,
        Direction::Decrypt,
        16,
    )
    .unwrap();
    dec.update_aad(aad);
    let mut decrypted = dec.update(&ciphertext[..32]).unwrap();
    decrypted.extend(
        dec.decrypt_final_with_input(Some(&ciphertext[32..]), &tag)
            .unwrap(),
    );
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_aes128_ctr_streaming_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ctr,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::None,
    )
    .unwrap();
    let ct = enc.update(b"CTR mode test").unwrap();

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ctr,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::None,
    )
    .unwrap();
    let pt = dec.update(&ct).unwrap();
    assert_eq!(&pt, b"CTR mode test");
}

#[test]
fn test_aes128_ctr_chunked_matches_one_shot() {
    let key = [0x42u8; 16];
    let iv = [0x11u8; 16];
    let plaintext = b"CTR input deliberately split at non-block offsets";

    let mut one_shot = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ctr,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::None,
    )
    .unwrap();
    let expected = one_shot.update(plaintext).unwrap();

    let mut chunked = StreamingCipherCtx::new(
        StreamingCipherAlgo::Aes128Ctr,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::None,
    )
    .unwrap();
    let mut actual = chunked.update(&plaintext[..7]).unwrap();
    actual.extend(chunked.update(&plaintext[7..23]).unwrap());
    actual.extend(chunked.update(&plaintext[23..]).unwrap());

    assert_eq!(actual, expected);
}

#[test]
fn test_sm4_gcm_chunked_non_block_aligned_matches_one_shot() {
    let key = [0x42u8; 16];
    let nonce = [0x24u8; 12];
    let aad = b"aad";
    let plaintext = b"GCM input deliberately split at non-block offsets";

    let mut one_shot = StreamingCipherCtx::new_aead(
        StreamingCipherAlgo::Sm4Gcm,
        &key,
        &nonce,
        Direction::Encrypt,
        16,
    )
    .unwrap();
    one_shot.update_aad(aad);
    let mut expected_ciphertext = one_shot.update(plaintext).unwrap();
    let (tail, expected_tag) = one_shot.encrypt_final().unwrap();
    expected_ciphertext.extend(tail);

    let mut chunked = StreamingCipherCtx::new_aead(
        StreamingCipherAlgo::Sm4Gcm,
        &key,
        &nonce,
        Direction::Encrypt,
        16,
    )
    .unwrap();
    chunked.update_aad(aad);
    let mut actual_ciphertext = chunked.update(&plaintext[..7]).unwrap();
    actual_ciphertext.extend(chunked.update(&plaintext[7..23]).unwrap());
    actual_ciphertext.extend(chunked.update(&plaintext[23..]).unwrap());
    let (tail, actual_tag) = chunked.encrypt_final().unwrap();
    actual_ciphertext.extend(tail);

    assert_eq!(actual_ciphertext, expected_ciphertext);
    assert_eq!(actual_tag, expected_tag);
}

#[test]
fn test_des3_cbc_streaming_roundtrip() {
    let key = [0x42u8; 24];
    let iv = [0x00u8; 8];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Des3Cbc,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"3DES CBC!").unwrap();
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Des3Cbc,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"3DES CBC!");
}

#[test]
fn test_des_ecb_streaming_roundtrip() {
    let key = [0x42u8; 8];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::DesEcb,
        &key,
        &[],
        Direction::Encrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut ct = enc.update(b"DES!!").unwrap();
    ct.extend(enc.r#final().unwrap());

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::DesEcb,
        &key,
        &[],
        Direction::Decrypt,
        PaddingMode::Pkcs7,
    )
    .unwrap();
    let mut pt = dec.update(&ct).unwrap();
    pt.extend(dec.r#final().unwrap());
    assert_eq!(&pt, b"DES!!");
}

#[test]
fn test_sm4_ctr_streaming_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];

    let mut enc = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Ctr,
        &key,
        &iv,
        Direction::Encrypt,
        PaddingMode::None,
    )
    .unwrap();
    let ct = enc.update(b"SM4 CTR test").unwrap();

    let mut dec = StreamingCipherCtx::new(
        StreamingCipherAlgo::Sm4Ctr,
        &key,
        &iv,
        Direction::Decrypt,
        PaddingMode::None,
    )
    .unwrap();
    let pt = dec.update(&ct).unwrap();
    assert_eq!(&pt, b"SM4 CTR test");
}
