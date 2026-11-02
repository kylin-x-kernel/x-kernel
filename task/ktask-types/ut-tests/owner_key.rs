// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use crate::*;

#[def_test]
fn from_token_pins_the_reservation_width() {
    assert_eq!(OwnerKey::from_token(1).unwrap().as_raw(), 8);
}

#[def_test]
fn token_round_trips_through_the_aligned_key() {
    for token in [1u64, 2, 3, 0x1234_5678, u64::MAX >> OwnerKey::FLAG_BITS] {
        let key = OwnerKey::from_token(token).unwrap();
        assert_eq!(key.as_raw() & OwnerKey::FLAG_MASK, 0);
        assert_eq!(key.as_raw(), token << OwnerKey::FLAG_BITS);
        assert_eq!(OwnerKey::from_aligned(key.as_raw()), Some(key));
    }
}

#[def_test]
fn zero_token_is_rejected() {
    assert_eq!(OwnerKey::from_token(0), None);
}

#[def_test]
fn overflowing_token_is_rejected_instead_of_wrapping() {
    let max_token = u64::MAX >> OwnerKey::FLAG_BITS;
    for token in [
        max_token + 1, // wraps to zero
        max_token + 2, // would wrap to the aligned key of token 1
        u64::MAX,
    ] {
        assert_eq!(OwnerKey::from_token(token), None);
    }
}

#[def_test]
fn aligned_key_with_flag_bits_set_is_rejected() {
    for raw in [1u64, OwnerKey::FLAG_MASK, OwnerKey::ALIGN | 1, u64::MAX] {
        assert_eq!(OwnerKey::from_aligned(raw), None);
    }
}

#[def_test]
fn zero_aligned_key_is_rejected() {
    assert_eq!(OwnerKey::from_aligned(0), None);
}

#[def_test]
fn aligned_key_with_clear_low_bits_is_accepted() {
    for raw in [OwnerKey::ALIGN, 0x1200_0080] {
        assert_eq!(OwnerKey::from_aligned(raw).unwrap().as_raw(), raw);
    }
}
