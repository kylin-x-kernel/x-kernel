// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::aead::*;

#[test]
fn test_aes128_gcm_roundtrip() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"associated data";
    let plaintext = b"hello, world! this is a secret message";

    let ciphertext = Aes128GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    // Ciphertext should be plaintext.len() + tag_size
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Aes128GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_aes128_gcm_tampered_ciphertext_fails() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"associated data";
    let plaintext = b"hello, world!";

    let mut ciphertext = Aes128GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    // Tamper with the first byte of the ciphertext
    ciphertext[0] ^= 0xFF;

    let result = Aes128GcmAead::decrypt(&key, &nonce, aad, &ciphertext);
    assert!(result.is_err());
}

#[test]
fn test_aes256_gcm_roundtrip() {
    let key = [0xABu8; 32];
    let nonce = [1u8; 12];
    let aad = b"aad";
    let plaintext = b"another secret";

    let ciphertext = Aes256GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Aes256GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_aes256_gcm_wrong_key_fails() {
    let key = [0xABu8; 32];
    let wrong_key = [0xCDu8; 32];
    let nonce = [1u8; 12];
    let aad = b"aad";
    let plaintext = b"secret";

    let ciphertext = Aes256GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    let result = Aes256GcmAead::decrypt(&wrong_key, &nonce, aad, &ciphertext);
    assert!(result.is_err());
}

#[test]
fn test_aes192_gcm_empty_plaintext_kat() {
    // optee_test regression_4005 AES-GCM VECT7 (24-byte key, empty payload)
    let key = [0u8; 24];
    let nonce = [0u8; 12];
    let tag_expect = [
        0xcd, 0x33, 0xb2, 0x8a, 0xc7, 0x73, 0xf7, 0x4b, 0xa0, 0x0e, 0xd1, 0xf3, 0x12, 0x57, 0x24,
        0x35,
    ];

    let ciphertext = Aes192GcmAead::encrypt(&key, &nonce, b"", b"").unwrap();
    assert_eq!(ciphertext.len(), 16);
    assert_eq!(ciphertext, tag_expect);

    let decrypted = Aes192GcmAead::decrypt(&key, &nonce, b"", &ciphertext).unwrap();
    assert!(decrypted.is_empty());
}

#[test]
fn test_aes192_gcm_roundtrip() {
    let key = [0x11u8; 24];
    let nonce = [0u8; 12];
    let aad = b"aad";
    let plaintext = b"aes-192-gcm payload";

    let ciphertext = Aes192GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Aes192GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_aead_sizes() {
    assert_eq!(Aes128GcmAead::key_size(), 16);
    assert_eq!(Aes192GcmAead::key_size(), 24);
    assert_eq!(Aes128GcmAead::nonce_size(), 12);
    assert_eq!(Aes128GcmAead::tag_size(), 16);
    assert_eq!(Aes256GcmAead::key_size(), 32);
    assert_eq!(Aes256GcmAead::nonce_size(), 12);
    assert_eq!(Aes256GcmAead::tag_size(), 16);
}

#[test]
fn test_aead_invalid_key_length() {
    let bad_key = [0u8; 8];
    let nonce = [0u8; 12];
    let result = Aes128GcmAead::encrypt(&bad_key, &nonce, b"", b"test");
    assert!(result.is_err());
}

#[test]
fn test_aead_invalid_nonce_length() {
    let key = [0u8; 16];
    // Empty nonce is invalid
    let result = Aes128GcmAead::encrypt(&key, &[], b"", b"test");
    assert!(result.is_err());
    // 17-byte nonce exceeds GCM limit
    let result = Aes128GcmAead::encrypt(&key, &[0u8; 17], b"", b"test");
    assert!(result.is_err());
}

#[test]
fn test_aes128_gcm_empty_plaintext() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"associated data";
    let plaintext: [u8; 0] = [];

    // Encrypting empty plaintext should still produce a tag (16 bytes)
    let ciphertext = Aes128GcmAead::encrypt(&key, &nonce, aad, &plaintext).unwrap();
    assert_eq!(ciphertext.len(), 16);

    let decrypted = Aes128GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted.len(), 0);
}

#[test]
fn test_sm4_gcm_roundtrip() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"associated data";
    let plaintext = b"hello SM4-GCM!";

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_sm4_gcm_tampered_ciphertext_fails() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"aad";
    let plaintext = b"secret";

    let mut ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    ciphertext[0] ^= 0xFF;

    let result = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext);
    assert!(result.is_err());
}

#[test]
fn test_sm4_gcm_empty_plaintext() {
    let key = [0x42u8; 16];
    let nonce = [0u8; 12];
    let aad = b"aad";
    let plaintext: [u8; 0] = [];

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, &plaintext).unwrap();
    assert_eq!(ciphertext.len(), 16);

    let decrypted = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted.len(), 0);
}

#[test]
fn test_sm4_gcm_sizes() {
    assert_eq!(Sm4GcmAead::key_size(), 16);
    assert_eq!(Sm4GcmAead::nonce_size(), 12);
    assert_eq!(Sm4GcmAead::tag_size(), 16);
}

#[test]
fn test_sm4_gcm_kat() {
    // RFC 8998 Section A.2 — SM4-GCM test vector
    let key = hex::decode("0123456789ABCDEFFEDCBA9876543210").unwrap();
    let nonce = hex::decode("00001234567800000000ABCD").unwrap();
    let aad = hex::decode("FEEDFACEDEADBEEFFEEDFACEDEADBEEFABADDAD2").unwrap();
    let plaintext = hex::decode(
        "AAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCDDDDDDDDDDDDDDDD\
             EEEEEEEEEEEEEEEEFFFFFFFFFFFFFFFFEEEEEEEEEEEEEEEEAAAAAAAAAAAAAAAA",
    )
    .unwrap();

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, &aad, &plaintext).unwrap();

    // Expected ciphertext from RFC 8998
    let expected_ct = hex::decode(
        "17F399F08C67D5EE19D0DC9969C4BB7D5FD46FD3756489069157B282BB200735\
             D82710CA5C22F0CCFA7CBF93D496AC15A56834CBCF98C397B4024A2691233B8D",
    )
    .unwrap();
    let expected_tag = hex::decode("83DE3541E4C2B58177E065A9BF7B62EC").unwrap();

    // Ciphertext should match expected
    assert_eq!(&ciphertext[..expected_ct.len()], &expected_ct);
    // Tag (last 16 bytes) should match expected
    assert_eq!(&ciphertext[ciphertext.len() - 16..], &expected_tag);
}

#[test]
fn test_sm4_gcm_wrong_key_fails() {
    let key = [0x42u8; 16];
    let wrong_key = [0x24u8; 16];
    let nonce = [0u8; 12];
    let aad = b"aad";
    let plaintext = b"secret";

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    let result = Sm4GcmAead::decrypt(&wrong_key, &nonce, aad, &ciphertext);
    assert!(result.is_err());
}

#[test]
fn test_sm4_gcm_16byte_nonce_roundtrip() {
    let key = [0x42u8; 16];
    let nonce = [0x01u8; 16];
    let aad = b"associated data";
    let plaintext = b"hello SM4-GCM with 16-byte nonce!";

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_sm4_gcm_16byte_nonce_tampered_ciphertext_fails() {
    let key = [0x42u8; 16];
    let nonce = [0x01u8; 16];
    let aad = b"aad";
    let plaintext = b"secret with 16-byte nonce";

    let mut ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    ciphertext[0] ^= 0xFF;

    let result = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext);
    assert!(result.is_err());
}

#[test]
fn test_sm4_gcm_16byte_nonce_empty_plaintext() {
    let key = [0x42u8; 16];
    let nonce = [0x01u8; 16];
    let aad = b"aad";
    let plaintext: [u8; 0] = [];

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, &plaintext).unwrap();
    assert_eq!(ciphertext.len(), 16);

    let decrypted = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted.len(), 0);
}

#[test]
fn test_sm4_gcm_13byte_nonce_roundtrip() {
    let key = [0x42u8; 16];
    let nonce = [0x05u8; 13];
    let aad = b"aad";
    let plaintext = b"13-byte nonce test";

    let ciphertext = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len() + 16);

    let decrypted = Sm4GcmAead::decrypt(&key, &nonce, aad, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_sm4_gcm_invalid_nonce_rejected() {
    let key = [0x42u8; 16];
    let aad = b"aad";
    let plaintext = b"test";

    // Empty nonce should be rejected
    let result = Sm4GcmAead::encrypt(&key, &[], aad, plaintext);
    assert!(result.is_err());

    // 17-byte nonce should be rejected
    let nonce = [0u8; 17];
    let result = Sm4GcmAead::encrypt(&key, &nonce, aad, plaintext);
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// AES-CCM tests
// -----------------------------------------------------------------------

#[test]
fn test_aes128_ccm_roundtrip() {
    let key = [0x40u8; 16];
    let nonce = [0x10u8; 7];
    let aad = [0u8; 8];
    let plaintext = b"hello CCM";

    let ct = ccm_encrypt::<aes::Aes128>(&key, &nonce, &aad, plaintext, 16).unwrap();
    assert_eq!(ct.len(), plaintext.len() + 16);

    let pt = ccm_decrypt::<aes::Aes128>(&key, &nonce, &aad, &ct, 16).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn test_aes128_ccm_short_tag_roundtrip() {
    let key = [0x40u8; 16];
    let nonce = [0x10u8; 7];
    let aad = [0u8; 8];
    let plaintext = b"hello CCM";

    let ct = ccm_encrypt::<aes::Aes128>(&key, &nonce, &aad, plaintext, 4).unwrap();
    assert_eq!(ct.len(), plaintext.len() + 4);

    let pt = ccm_decrypt::<aes::Aes128>(&key, &nonce, &aad, &ct, 4).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn test_aes128_ccm_tampered_fails() {
    let key = [0x40u8; 16];
    let nonce = [0x10u8; 7];
    let aad = [0u8; 8];
    let plaintext = b"hello CCM";

    let mut ct = ccm_encrypt::<aes::Aes128>(&key, &nonce, &aad, plaintext, 8).unwrap();
    ct[0] ^= 0xFF;
    assert!(ccm_decrypt::<aes::Aes128>(&key, &nonce, &aad, &ct, 8).is_err());
}

#[test]
fn test_aes256_ccm_roundtrip() {
    let key = [0xABu8; 32];
    let nonce = [0x01u8; 12];
    let aad = b"aad";
    let plaintext = b"AES-256-CCM streaming";

    let ct = ccm_encrypt::<aes::Aes256>(&key, &nonce, aad, plaintext, 16).unwrap();
    assert_eq!(ct.len(), plaintext.len() + 16);

    let pt = ccm_decrypt::<aes::Aes256>(&key, &nonce, aad, &ct, 16).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn test_aes128_ccm_kat() {
    // Test vector matching the kernel test
    let key = [
        0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e,
        0x4f,
    ];
    let nonce = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
    let aad = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
    let plain = [0x20, 0x21, 0x22, 0x23];
    let cipher_expect = [0x71, 0x62, 0x01, 0x5b];
    let tag_expect = [0x4d, 0xac, 0x25, 0x5d];

    let ct = ccm_encrypt::<aes::Aes128>(&key, &nonce, &aad, &plain, 4).unwrap();
    assert_eq!(&ct[..4], &cipher_expect);
    assert_eq!(&ct[4..], &tag_expect);
}
