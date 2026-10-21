// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn test_b2hs_empty_input() {
    let b = &[];
    let mut hs = [0u8; 1];
    let result = tee_b2hs(b, &mut hs);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);
    assert_eq!(hs[0], 0);
}

#[unittest::def_test]
fn test_b2hs_single_byte() {
    let b = &[0xAB];
    let mut hs = [0u8; 3];
    let result = tee_b2hs(b, &mut hs);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 2);
    assert_eq!(str::from_utf8(&hs[..2]).unwrap(), "AB");
    assert_eq!(hs[2], 0);
}

#[unittest::def_test]
fn test_b2hs_multiple_bytes() {
    let b = &[0x12, 0x34, 0xCD, 0xEF];
    let mut hs = [0u8; 9];
    let result = tee_b2hs(b, &mut hs);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 8);
    assert_eq!(str::from_utf8(&hs[..8]).unwrap(), "1234CDEF");
    assert_eq!(hs[8], 0);
}

#[unittest::def_test]
fn test_b2hs_short_buffer() {
    let b = &[0x12, 0x34];
    let mut hs = [0u8; 4];
    let result = tee_b2hs(b, &mut hs);
    assert!(result.is_err());
}

#[unittest::def_test]
fn test_b2hs_exact_buffer_size() {
    let b = &[0xAA];
    let mut hs = [0u8; 3];
    let result = tee_b2hs(b, &mut hs);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 2);
    assert_eq!(str::from_utf8(&hs[..2]).unwrap(), "AA");
    assert_eq!(hs[2], 0);
}

#[unittest::def_test]
fn test_hs2b_empty_input() {
    let hs = &[];
    let mut b = [0u8; 0];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);
}

#[unittest::def_test]
fn test_hs2b_single_byte_hex() {
    let hs = b"AB";
    let mut b = [0u8; 1];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 1);
    assert_eq!(b[0], 0xAB);
}

#[unittest::def_test]
fn test_hs2b_multiple_bytes_hex() {
    let hs = b"1234cdef";
    let mut b = [0u8; 4];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 4);
    assert_eq!(b, [0x12, 0x34, 0xCD, 0xEF]);
}

#[unittest::def_test]
fn test_hs2b_odd_length_hex() {
    let hs = b"123";
    let mut b = [0u8; 2];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_err());
}

#[unittest::def_test]
fn test_hs2b_short_buffer() {
    let hs = b"1234";
    let mut b = [0u8; 1];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_err());
}

#[unittest::def_test]
fn test_hs2b_invalid_hex_chars() {
    let hs = b"12gx";
    let mut b = [0u8; 2];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_err());
}

#[unittest::def_test]
fn test_hs2b_uppercase_hex() {
    let hs = b"ABCDEF";
    let mut b = [0u8; 3];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 3);
    assert_eq!(b, [0xAB, 0xCD, 0xEF]);
}

#[unittest::def_test]
fn test_hs2b_mixed_case_hex() {
    let hs = b"aBcDeF";
    let mut b = [0u8; 3];
    let result = tee_hs2b(hs, &mut b);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 3);
    assert_eq!(b, [0xAB, 0xCD, 0xEF]);
}
