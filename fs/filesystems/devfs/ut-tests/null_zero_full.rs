// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_null_read_returns_zero() {
    let dev = Null;
    let mut buf = [0xFFu8; 64];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_null_write_accepts_all() {
    let dev = Null;
    let data = [1u8; 128];
    let n = dev.write_bytes(&data).unwrap();
    assert_eq!(n, 128);
}

#[def_test]
fn test_zero_read_fills_zeros() {
    let dev = Zero;
    let mut buf = [0xFFu8; 32];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 32);
    assert!(buf.iter().all(|&b| b == 0));
}

#[def_test]
fn test_zero_write_accepts_all() {
    let dev = Zero;
    let data = [42u8; 64];
    let n = dev.write_bytes(&data).unwrap();
    assert_eq!(n, 64);
}

#[def_test]
fn test_full_read_fills_zeros() {
    let dev = Full;
    let mut buf = [0xFFu8; 16];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 16);
    assert!(buf.iter().all(|&b| b == 0));
}

#[def_test]
fn test_full_write_returns_error() {
    let dev = Full;
    let data = [1u8; 8];
    assert!(dev.write_bytes(&data).is_err());
}

#[def_test]
fn test_null_flags() {
    let dev = Null;
    let flags = dev.flags();
    assert!(flags.contains(NodeFlags::NON_CACHEABLE));
}

#[def_test]
fn test_zero_flags() {
    let dev = Zero;
    let flags = dev.flags();
    assert!(flags.contains(NodeFlags::NON_CACHEABLE));
}
