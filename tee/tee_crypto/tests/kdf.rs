// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::kdf::pbkdf2_hmac_sm3;

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
