// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;

#[def_test]
fn test_smccc_trng_read_zero_len() {
    assert!(read(0).is_err());
}

#[def_test]
fn test_smccc_trng_read_when_available() {
    init();
    if is_available() {
        let data = read(32).expect("SMCCC TRNG should return bytes when available");
        assert_eq!(data.len(), 32);
        assert_ne!(data.as_slice(), &[0u8; 32]);
    } else {
        assert!(read(32).is_err());
    }
}
