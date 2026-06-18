// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::block_cipher::*;

#[test]
fn test_aes128_ecb_nist() {
    // NIST SP 800-38A test vector (F.1.1)
    let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
    let mut block = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
    Aes128Ecb::encrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block), "3ad77bb40d7a3660a89ecaf32466ef97");

    // Decrypt should round-trip
    Aes128Ecb::decrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block), "6bc1bee22e409f96e93d7e117393172a");
}

#[test]
fn test_sm4_ecb_standard() {
    // SM4 standard test vector (GB/T 32907-2016 Example 1)
    let key = hex::decode("0123456789abcdeffedcba9876543210").unwrap();
    let mut block = hex::decode("0123456789abcdeffedcba9876543210").unwrap();
    Sm4Ecb::encrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block), "681edf34d206965e86b3e94f536e4246");

    // Decrypt should round-trip
    Sm4Ecb::decrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block), "0123456789abcdeffedcba9876543210");
}

#[test]
fn test_aes256_ecb_round_trip() {
    let key = [0x42u8; 32];
    let original = [0xABu8; 16];
    let mut block = original;
    Aes256Ecb::encrypt(&key, &mut block).unwrap();
    assert_ne!(block, original);
    Aes256Ecb::decrypt(&key, &mut block).unwrap();
    assert_eq!(block, original);
}

#[test]
fn test_block_and_key_sizes() {
    assert_eq!(Aes128Ecb::block_size(), 16);
    assert_eq!(Aes128Ecb::key_size(), 16);
    assert_eq!(Aes256Ecb::block_size(), 16);
    assert_eq!(Aes256Ecb::key_size(), 32);
    assert_eq!(Sm4Ecb::block_size(), 16);
    assert_eq!(Sm4Ecb::key_size(), 16);
}

#[test]
fn test_des_ecb_nist() {
    // NIST DES test vector
    let key = hex::decode("0123456789ABCDEF").unwrap();
    let mut block = hex::decode("4E6F772069732074").unwrap();
    DesEcb::encrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block).to_uppercase(), "3FA40E8A984D4815");
    DesEcb::decrypt(&key, &mut block).unwrap();
    assert_eq!(hex::encode(&block).to_uppercase(), "4E6F772069732074");
}

#[test]
fn test_des3_ecb_roundtrip() {
    let key = [0x42u8; 24];
    let original = [0xABu8; 8];
    let mut block = original;
    Des3Ecb::encrypt(&key, &mut block).unwrap();
    assert_ne!(block, original);
    Des3Ecb::decrypt(&key, &mut block).unwrap();
    assert_eq!(block, original);
}

#[test]
fn test_des_sizes() {
    assert_eq!(DesEcb::block_size(), 8);
    assert_eq!(DesEcb::key_size(), 8);
    assert_eq!(Des3Ecb::block_size(), 8);
    assert_eq!(Des3Ecb::key_size(), 24);
}
