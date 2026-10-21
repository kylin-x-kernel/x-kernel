// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{
    hash::{HashAlgorithm, Sha256},
    kdf::{concat_kdf, concat_kdf_digest, pbkdf2_hmac_sha1, pbkdf2_hmac_sm3},
};

#[test]
fn pbkdf2_hmac_sha1_rfc6070_test_case_1() {
    let dk = pbkdf2_hmac_sha1(b"password", b"salt", 1, 20).expect("pbkdf2");
    let expected = hex::decode("0c60c80f961f0e71f3a9b524af6012062fe037a6").unwrap();
    assert_eq!(dk, expected);
}

#[test]
fn pbkdf2_hmac_sha1_rfc6070_test_case_3() {
    let dk = pbkdf2_hmac_sha1(b"password", b"salt", 4096, 20).expect("pbkdf2");
    let expected = hex::decode("4b007901b765489abead49d926f721d065a429c1").unwrap();
    assert_eq!(dk, expected);
}

#[test]
fn pbkdf2_hmac_sha1_rejects_zero_iterations() {
    assert!(pbkdf2_hmac_sha1(b"password", b"salt", 0, 20).is_err());
}

#[test]
fn pbkdf2_hmac_sha1_is_deterministic() {
    let a = pbkdf2_hmac_sha1(b"pass", b"salt", 2048, 16).expect("a");
    let b = pbkdf2_hmac_sha1(b"pass", b"salt", 2048, 16).expect("b");
    assert_eq!(a, b);
}

#[test]
fn pbkdf2_hmac_sm3_derives_expected_length() {
    let dk = pbkdf2_hmac_sm3(b"password", b"salt", 1000, 32).expect("pbkdf2");
    assert_eq!(dk.len(), 32);
}

#[test]
fn pbkdf2_hmac_sm3_rejects_zero_iterations() {
    assert!(pbkdf2_hmac_sm3(b"password", b"salt", 0, 32).is_err());
}

#[test]
fn pbkdf2_hmac_sm3_is_deterministic() {
    let a = pbkdf2_hmac_sm3(b"pass", b"salt", 2048, 16).expect("a");
    let b = pbkdf2_hmac_sm3(b"pass", b"salt", 2048, 16).expect("b");
    assert_eq!(a, b);
}

#[test]
fn concat_kdf_rejects_zero_output_length() {
    assert!(concat_kdf(HashAlgorithm::Sha256, b"z", b"info", 0).is_err());
    assert!(concat_kdf_digest::<Sha256>(b"z", b"info", 0).is_err());
}

#[test]
fn concat_kdf_sha256_single_block() {
    let z = b"abc";
    let info = b"AlgorithmIDPartyUInfoPartyVInfo";
    let dk = concat_kdf_digest::<Sha256>(z, info, 32).expect("concat kdf");
    assert_eq!(dk.len(), 32);
    let expected =
        hex::decode("67dec0497920d4f26b6677a18a080d699358296100bd73d09305bbebb4ffc17c").unwrap();
    assert_eq!(dk, expected);
}

#[test]
fn concat_kdf_sha256_multi_block() {
    let z = [
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b,
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b,
    ];
    let info = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
    let dk_len = 42usize;

    let dk = concat_kdf(HashAlgorithm::Sha256, &z, &info, dk_len).expect("concat kdf");
    assert_eq!(dk.len(), dk_len);

    let block1 = concat_kdf_digest::<Sha256>(&z, &info, 32).expect("block1");
    let block2 = concat_kdf_digest::<Sha256>(&z, &info, 42).expect("block2");
    assert_eq!(&block2[..32], block1.as_slice());
    assert_ne!(&block2[32..], &block1[..10.min(block1.len())]);
}

#[test]
fn concat_kdf_empty_other_info() {
    let dk = concat_kdf(HashAlgorithm::Sha256, b"shared secret", &[], 32).expect("concat kdf");
    assert_eq!(dk.len(), 32);
    let again = concat_kdf_digest::<Sha256>(b"shared secret", &[], 32).expect("concat kdf");
    assert_eq!(dk, again);
}

#[test]
fn concat_kdf_exact_multiple_of_hash_length() {
    let dk = concat_kdf(HashAlgorithm::Sha256, b"material", b"ctx", 64).expect("concat kdf");
    assert_eq!(dk.len(), 64);
    assert_ne!(&dk[..32], &dk[32..]);
}

#[test]
fn concat_kdf_unsupported_hash_algorithm() {
    assert!(concat_kdf(HashAlgorithm::Sm3, b"z", b"info", 32).is_err());
}
