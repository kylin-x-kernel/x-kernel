// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;

#[def_test]
fn test_random_read_fills_buffer() {
    let dev = Random::new();
    let mut buf = [0u8; 64];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 64);
    assert!(buf.iter().any(|&b| b != 0));
}

#[def_test]
fn test_random_read_empty() {
    let dev = Random::new();
    let mut buf = [];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_random_write_accepts_all() {
    let dev = Random::new();
    let data = [0u8; 32];
    let n = dev.write_bytes(&data).unwrap();
    assert_eq!(n, 32);
}

#[def_test]
fn test_random_write_empty() {
    let dev = Random::new();
    let n = dev.write_bytes(&[]).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_random_supports_rw() {
    let dev = Random::new();
    assert!(dev.supports_read());
    assert!(dev.supports_write());
    assert!(dev.flags().contains(NodeFlags::NON_CACHEABLE));
}

#[def_test]
fn test_random_two_reads_differ() {
    let dev = Random::new();
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    dev.read_bytes(&mut buf1).unwrap();
    dev.read_bytes(&mut buf2).unwrap();
    assert_ne!(buf1, buf2);
}

#[def_test]
fn test_random_write_changes_stream() {
    let dev = Random::new();
    let mut before = [0u8; 32];
    let mut after = [0u8; 32];
    entropy::init();
    dev.read_bytes(&mut before).unwrap();
    dev.write_bytes(b"entropy").unwrap();
    dev.read_bytes(&mut after).unwrap();
    assert_ne!(before, after);
}
