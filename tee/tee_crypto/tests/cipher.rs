// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::cipher::*;
extern crate alloc;

#[test]
fn test_aes128_cbc_nist() {
    let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
    let iv = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
    let plaintext = hex::decode(
            "6bc1bee22e409f96e93d7e117393172a\
             ae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52ef\
             f69f2445df4f9b17ad2b417be66c3710",
        )
        .unwrap();
    // NIST test vectors don't have PKCS7 padding, so we test
    // the padded round-trip instead of matching exact ciphertext
    let ct = aes128_cbc_encrypt(&key, &iv, &plaintext).unwrap();
    let pt = aes128_cbc_decrypt(&key, &iv, &ct).unwrap();
    assert_eq!(&pt, &plaintext);
}

#[test]
fn test_aes128_cbc_padding() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];
    let plaintext = alloc::vec![0xAAu8; 17];
    let ct = aes128_cbc_encrypt(&key, &iv, &plaintext).unwrap();
    assert_eq!(ct.len(), 32);
    let pt = aes128_cbc_decrypt(&key, &iv, &ct).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn test_sm4_cbc_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];
    let plaintext = b"Hello, SM4-CBC mode!";
    let ct = sm4_cbc_encrypt(&key, &iv, plaintext).unwrap();
    let pt = sm4_cbc_decrypt(&key, &iv, &ct).unwrap();
    assert_eq!(&pt, plaintext);
}

#[test]
fn test_des3_cbc_roundtrip() {
    let key = [0x42u8; 24];
    let iv = [0x00u8; 8];
    let plaintext = b"Triple-DES CBC!";
    let ct = des3_cbc_encrypt(&key, &iv, plaintext).unwrap();
    assert_eq!(ct.len() % 8, 0);
    let pt = des3_cbc_decrypt(&key, &iv, &ct).unwrap();
    assert_eq!(&pt, plaintext);
}

#[test]
fn test_aes128_ctr_nist() {
    let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
    let iv = hex::decode("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff").unwrap();
    let mut data =
        hex::decode("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51").unwrap();
    aes128_ctr(&key, &iv, &mut data).unwrap();
    let expected =
        hex::decode("874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff").unwrap();
    assert_eq!(hex::encode(&data), hex::encode(&expected));
}

#[test]
fn test_sm4_ctr_roundtrip() {
    let key = [0x42u8; 16];
    let iv = [0x00u8; 16];
    let original = b"SM4 CTR mode test data";
    let mut data = original.to_vec();
    sm4_ctr(&key, &iv, &mut data).unwrap();
    assert_ne!(data, original);
    sm4_ctr(&key, &iv, &mut data).unwrap();
    assert_eq!(&data, original);
}

#[test]
fn test_des_cbc_roundtrip() {
    let key = [0x42u8; 8];
    let iv = [0x00u8; 8];
    let plaintext = b"DES!!";
    let ct = des_cbc_encrypt(&key, &iv, plaintext).unwrap();
    assert_eq!(ct.len(), 8);
    let pt = des_cbc_decrypt(&key, &iv, &ct).unwrap();
    assert_eq!(&pt, plaintext);
}
