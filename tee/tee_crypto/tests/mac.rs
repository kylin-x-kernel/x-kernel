// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::mac::*;
extern crate alloc;

fn hex_encode(data: &[u8]) -> alloc::string::String {
    hex::encode(data)
}

#[test]
fn test_hmac_sha256_rfc4231_test_case_2() {
    // RFC 4231 Test Case 2
    let mut mac = HmacSha256::new(b"Jefe").unwrap();
    mac.update(b"what do ya want for nothing?");
    let result = mac.finalize();
    assert_eq!(
        hex_encode(&result),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn test_hmac_md5_rfc2202() {
    let mut mac = HmacMd5::new(b"Jefe").unwrap();
    mac.update(b"what do ya want for nothing?");
    let result = mac.finalize();
    assert_eq!(hex_encode(&result), "750c783e6ab0b503eaa86e310a5db738");
}

#[test]
fn test_aes192_cmac_empty_vect5() {
    let key = [
        0x8e, 0x73, 0xb0, 0xf7, 0xda, 0x0e, 0x64, 0x52, 0xc8, 0x10, 0xf3, 0x2b, 0x80, 0x90, 0x79,
        0xe5, 0x62, 0xf8, 0xea, 0xd2, 0x52, 0x2c, 0x6b, 0x7b,
    ];
    let mac = Aes192Cmac::new(&key).unwrap();
    let result = mac.finalize();
    assert_eq!(hex_encode(&result), "d17ddf46adaacde531cac483de7a9367");
}

#[test]
fn test_hmac_output_sizes() {
    assert_eq!(HmacMd5::output_size(), 16);
    assert_eq!(HmacSha1::output_size(), 20);
    assert_eq!(HmacSha224::output_size(), 28);
    assert_eq!(HmacSha256::output_size(), 32);
    assert_eq!(HmacSha384::output_size(), 48);
    assert_eq!(HmacSha512::output_size(), 64);
    assert_eq!(HmacSm3::output_size(), 32);
    assert_eq!(Aes128Cmac::output_size(), 16);
    assert_eq!(Aes192Cmac::output_size(), 16);
    assert_eq!(Aes256Cmac::output_size(), 16);
    assert_eq!(Sm4Cmac::output_size(), 16);
    assert_eq!(Des3Cmac::output_size(), 8);
}

#[test]
fn test_hmac_sha512_produces_output() {
    let mut mac = HmacSha512::new(b"key").unwrap();
    mac.update(b"data");
    let result = mac.finalize();
    assert_eq!(result.len(), 64);
}

#[test]
fn test_hmac_sm3_produces_output() {
    let mut mac = HmacSm3::new(b"key").unwrap();
    mac.update(b"data");
    let result = mac.finalize();
    assert_eq!(result.len(), 32);
}

#[test]
fn test_aes128_cmac_rfc() {
    // RFC 4493 Test Case 1: key=2b7e151628aed2a6abf7158809cf4f3c, M=<empty>
    let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
    let mac = Aes128Cmac::new(&key).unwrap();
    let result = mac.finalize();
    assert_eq!(hex_encode(&result), "bb1d6929e95937287fa37d129b756746");
}

#[test]
fn test_aes256_cmac_produces_output() {
    let key = [0u8; 32];
    let mac = Aes256Cmac::new(&key).unwrap();
    let result = mac.finalize();
    assert_eq!(result.len(), 16);
}

#[test]
fn test_sm4_cmac_produces_output() {
    let key = [0u8; 16];
    let mac = Sm4Cmac::new(&key).unwrap();
    let result = mac.finalize();
    assert_eq!(result.len(), 16);
}

#[test]
fn test_invalid_key_length_hmac() {
    // HMAC accepts any key length, so this should succeed.
    let result = HmacSha256::new(b"short");
    assert!(result.is_ok());
}

#[test]
fn test_invalid_key_length_cmac() {
    // CMAC with wrong key length should fail.
    let result = Aes128Cmac::new(b"short");
    assert!(result.is_err());
}

#[test]
fn test_incremental_update() {
    let mut mac1 = HmacSha256::new(b"key").unwrap();
    mac1.update(b"hello");
    mac1.update(b" world");

    let mut mac2 = HmacSha256::new(b"key").unwrap();
    mac2.update(b"hello world");

    assert_eq!(mac1.finalize(), mac2.finalize());
}

#[test]
fn test_des3_cmac_produces_output() {
    let key = [0u8; 24];
    let mac = Des3Cmac::new(&key).unwrap();
    let result = mac.finalize();
    assert_eq!(result.len(), 8);
}

#[test]
fn test_des3_cmac_roundtrip() {
    let key = [0x42u8; 24];
    let mut mac1 = Des3Cmac::new(&key).unwrap();
    mac1.update(b"hello");
    mac1.update(b" world");

    let mut mac2 = Des3Cmac::new(&key).unwrap();
    mac2.update(b"hello world");

    assert_eq!(mac1.finalize(), mac2.finalize());
}

#[test]
fn test_des3_cmac_different_keys_differ() {
    let key1 = [0x01u8; 24];
    let key2 = [0x02u8; 24];
    let mut mac1 = Des3Cmac::new(&key1).unwrap();
    mac1.update(b"test");
    let mut mac2 = Des3Cmac::new(&key2).unwrap();
    mac2.update(b"test");
    assert_ne!(mac1.finalize(), mac2.finalize());
}
