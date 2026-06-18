// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{hkdf::*, mac::HmacSha256};

fn hex(data: &[u8]) -> String {
    hex::encode(data)
}

#[test]
fn test_hkdf_rfc5869_test_case_1() {
    // RFC 5869 Test Case 1 — HMAC-SHA256
    let ikm = hex::decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b").unwrap();
    let salt = hex::decode("000102030405060708090a0b0c").unwrap();
    let info = hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap();
    let l = 42;

    let okm = hkdf::<HmacSha256>(&salt, &ikm, &info, l).unwrap();
    let expected = hex::decode(
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
    )
    .unwrap();
    assert_eq!(hex(&okm[..expected.len()]), hex(&expected));
}

#[test]
fn test_hkdf_empty_salt() {
    let ikm = b"input key material";
    let info = b"info";
    let okm = hkdf::<HmacSha256>(&[], ikm, info, 32).unwrap();
    assert_eq!(okm.len(), 32);
}

#[test]
fn test_hkdf_empty_info() {
    let okm = hkdf::<HmacSha256>(b"salt", b"ikm", b"", 32).unwrap();
    assert_eq!(okm.len(), 32);
}

#[test]
fn test_hkdf_multi_block_output() {
    let okm = hkdf::<HmacSha256>(b"salt", b"ikm", b"info", 64).unwrap();
    assert_eq!(okm.len(), 64);
    // First and second halves should differ
    assert_ne!(&okm[..32], &okm[32..]);
}
