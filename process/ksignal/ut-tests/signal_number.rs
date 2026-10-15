// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regression coverage for checked raw signal construction.

use unittest::{assert_eq, def_test};

use crate::Signo;

#[def_test]
fn test_raw_unsigned_signal_number_rejects_truncation_aliases() {
    for raw in 1_u32..=64 {
        assert_eq!(Signo::from_raw(raw).map(|value| value as u32), Some(raw));
    }
    for raw in [0, 65, 255, 256, 257, 320, 65_537, u32::MAX - 254, u32::MAX] {
        assert_eq!(Signo::from_raw(raw), None);
    }
}

#[def_test]
fn test_raw_signed_signal_number_rejects_truncation_aliases() {
    for raw in 1_i32..=64 {
        assert_eq!(Signo::from_raw(raw).map(|value| value as i32), Some(raw));
    }
    for raw in [
        i32::MIN,
        -65_535,
        -255,
        -1,
        0,
        65,
        255,
        256,
        257,
        320,
        65_537,
        i32::MAX - 254,
        i32::MAX,
    ] {
        assert_eq!(Signo::from_raw(raw), None);
    }
}

#[def_test]
fn test_raw_wide_signal_number_rejects_truncation_aliases() {
    for raw in 1_u64..=64 {
        assert_eq!(Signo::from_raw(raw).map(|value| value as u64), Some(raw));
    }
    for raw in [
        0,
        65,
        255,
        256,
        257,
        320,
        65_537,
        1_u64 << 32,
        (1_u64 << 32) | 1,
        u64::from(u32::MAX),
        (1_u64 << 63) - 1,
        1_u64 << 63,
        (1_u64 << 63) | 1,
        u64::MAX - 254,
        u64::MAX,
    ] {
        assert_eq!(Signo::from_raw(raw), None);
    }
}
