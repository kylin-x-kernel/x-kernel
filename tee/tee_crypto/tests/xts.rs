// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::xts::*;

#[test]
fn test_aes128_xts_roundtrip() {
    let key = [0x42u8; 32]; // 16 data key + 16 tweak key
    let tweak = [0u8; 16];
    let original = [0xABu8; 64];
    let mut data = original;

    Aes128Xts::encrypt(&key, &tweak, &mut data).unwrap();
    assert_ne!(data, original);

    Aes128Xts::decrypt(&key, &tweak, &mut data).unwrap();
    assert_eq!(data, original);
}

#[test]
fn test_aes256_xts_roundtrip() {
    let key = [0x55u8; 64]; // 32 data key + 32 tweak key
    let tweak = [1u8; 16];
    let original = [0xCDu8; 48];
    let mut data = original;

    Aes256Xts::encrypt(&key, &tweak, &mut data).unwrap();
    assert_ne!(data, original);

    Aes256Xts::decrypt(&key, &tweak, &mut data).unwrap();
    assert_eq!(data, original);
}

#[test]
fn test_sm4_xts_roundtrip() {
    let key = [0x33u8; 32];
    let tweak = [2u8; 16];
    let original = [0xEFu8; 48];
    let mut data = original;

    Sm4Xts::encrypt(&key, &tweak, &mut data).unwrap();
    assert_ne!(data, original);

    Sm4Xts::decrypt(&key, &tweak, &mut data).unwrap();
    assert_eq!(data, original);
}

#[test]
fn test_xts_single_block() {
    let key = [0x42u8; 32];
    let tweak = [0u8; 16];
    let original = [0x01u8; 16];
    let mut data = original;

    Aes128Xts::encrypt(&key, &tweak, &mut data).unwrap();
    assert_ne!(data, original);

    Aes128Xts::decrypt(&key, &tweak, &mut data).unwrap();
    assert_eq!(data, original);
}

#[test]
fn test_xts_partial_block() {
    // 20 bytes = 1 full block + 4 bytes remainder (ciphertext stealing)
    let key = [0x42u8; 32];
    let tweak = [0u8; 16];
    let original = [0xABu8; 20];
    let mut data = original;

    Aes128Xts::encrypt(&key, &tweak, &mut data).unwrap();
    assert_ne!(data, original);

    Aes128Xts::decrypt(&key, &tweak, &mut data).unwrap();
    assert_eq!(data, original);
}

#[test]
fn test_xts_key_sizes() {
    assert_eq!(Aes128Xts::key_size(), 32);
    assert_eq!(Aes256Xts::key_size(), 64);
    assert_eq!(Sm4Xts::key_size(), 32);
}

#[test]
fn test_xts_invalid_key() {
    let bad_key = [0u8; 16]; // too short
    let tweak = [0u8; 16];
    let mut data = [0u8; 32];
    let result = Aes128Xts::encrypt(&bad_key, &tweak, &mut data);
    assert!(result.is_err());
}

#[test]
fn test_xts_invalid_tweak() {
    let key = [0u8; 32];
    let bad_tweak = [0u8; 8]; // wrong length
    let mut data = [0u8; 32];
    let result = Aes128Xts::encrypt(&key, &bad_tweak, &mut data);
    assert!(result.is_err());
}

#[test]
fn test_xts_data_too_small() {
    let key = [0u8; 32];
    let tweak = [0u8; 16];
    let mut data = [0u8; 8]; // less than one block
    let result = Aes128Xts::encrypt(&key, &tweak, &mut data);
    assert!(result.is_err());
}
