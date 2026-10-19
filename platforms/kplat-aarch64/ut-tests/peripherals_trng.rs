// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn success_result() -> SmcccResult {
    SmcccResult {
        x0: SMCCC_RET_SUCCESS,
        x1: 0x1111_2222_3333_4444,
        x2: 0x5555_6666_7777_8888,
        x3: 0x9999_aaaa_bbbb_cccc,
    }
}

#[def_test]
fn test_read_trng_random_with_reports_no_entropy_before_data() {
    let mut buf = [0u8; 32];
    let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
        x0: SMCCC_RET_TRNG_NO_ENTROPY as usize,
        ..SmcccResult::default()
    })
    .err();
    assert_eq!(err, Some(TrngReadError::NoEntropy));
}

#[def_test]
fn test_read_trng_random_with_reports_invalid_parameter_before_data() {
    let mut buf = [0u8; 32];
    let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
        x0: SMCCC_RET_TRNG_INVALID_PARAMETER as usize,
        ..SmcccResult::default()
    })
    .err();
    assert_eq!(err, Some(TrngReadError::InvalidParameter));
}

#[def_test]
fn test_read_trng_random_with_reports_unknown_error_before_data() {
    let mut buf = [0u8; 32];
    let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
        x0: (-99isize) as usize,
        ..SmcccResult::default()
    })
    .err();
    assert_eq!(err, Some(TrngReadError::Failed));
}

#[def_test]
fn test_read_trng_random_with_keeps_partial_bytes_on_no_entropy() {
    let mut calls = 0;
    let mut max_bits_seen = 0;
    let mut buf = [0u8; 32];
    let read = read_trng_random_with(&mut buf, |_, bits| {
        max_bits_seen = max_bits_seen.max(bits);
        calls += 1;
        if calls == 1 {
            success_result()
        } else {
            SmcccResult {
                x0: SMCCC_RET_TRNG_NO_ENTROPY as usize,
                ..SmcccResult::default()
            }
        }
    })
    .expect("partial data should be returned");
    assert_eq!(read, MAX_BITS_PER_CALL / 8);
    assert_eq!(calls, 2);
    assert!(max_bits_seen <= MAX_BITS_PER_CALL);
}

#[def_test]
fn test_read_trng_random_with_keeps_partial_bytes_on_invalid_parameter() {
    let mut calls = 0;
    let mut buf = [0u8; 32];
    let read = read_trng_random_with(&mut buf, |_, _| {
        calls += 1;
        if calls == 1 {
            success_result()
        } else {
            SmcccResult {
                x0: SMCCC_RET_TRNG_INVALID_PARAMETER as usize,
                ..SmcccResult::default()
            }
        }
    })
    .expect("partial data should be returned");
    assert_eq!(read, MAX_BITS_PER_CALL / 8);
}

#[def_test]
fn test_read_trng_random_with_keeps_partial_bytes_on_unknown_error() {
    let mut calls = 0;
    let mut buf = [0u8; 32];
    let read = read_trng_random_with(&mut buf, |_, _| {
        calls += 1;
        if calls == 1 {
            success_result()
        } else {
            SmcccResult {
                x0: (-99isize) as usize,
                ..SmcccResult::default()
            }
        }
    })
    .expect("partial data should be returned");
    assert_eq!(read, MAX_BITS_PER_CALL / 8);
}
