// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn pool_allocates_fixed_slots_and_recycles() {
    let pool = VsockTxBufPool::new(2, 64).unwrap();
    let first = pool.alloc_buf().unwrap();
    let second = pool.alloc_buf().unwrap();

    assert_eq!(pool.capacity(), 2);
    assert_eq!(pool.buffer_len(), 64);
    assert_eq!(first.capacity(), 64);
    assert!(pool.alloc_buf().is_none());

    drop(first);
    let recycled = pool.alloc_buf().unwrap();
    assert_eq!(recycled.capacity(), 64);
    drop(recycled);
    drop(second);
}

#[unittest::def_test]
fn buffer_tracks_header_payload_and_frame() {
    let pool = VsockTxBufPool::new(1, 32).unwrap();
    let mut buf = pool.alloc_buf().unwrap();

    buf.set_hdr_len(8).unwrap();
    buf.set_payload_len(4).unwrap();
    buf.header_mut().fill(0xa5);
    buf.payload_mut().copy_from_slice(&[1, 2, 3, 4]);

    assert_eq!(buf.payload_capacity(), 24);
    assert_eq!(buf.header(), &[0xa5; 8]);
    assert_eq!(buf.payload(), &[1, 2, 3, 4]);
    assert_eq!(&buf.frame()[..8], &[0xa5; 8]);
    assert_eq!(&buf.frame()[8..], &[1, 2, 3, 4]);
}

#[unittest::def_test]
fn buffer_rejects_overflow_and_reset_clears_lengths() {
    let pool = VsockTxBufPool::new(1, 16).unwrap();
    let mut buf = pool.alloc_buf().unwrap();

    buf.set_hdr_len(8).unwrap();
    assert!(buf.set_payload_len(9).is_err());
    buf.set_payload_len(8).unwrap();
    assert_eq!(buf.frame().len(), 16);

    buf.reset();
    assert_eq!(buf.hdr_len(), 0);
    assert_eq!(buf.payload_len(), 0);
    assert_eq!(buf.payload_capacity(), 16);
}

#[unittest::def_test]
fn pool_rejects_invalid_geometry() {
    assert!(VsockTxBufPool::new(0, 64).is_err());
    assert!(VsockTxBufPool::new(1, 0).is_err());
    let pool = VsockTxBufPool::new(1, 8).unwrap();
    let mut buf = pool.alloc_buf().unwrap();
    buf.set_payload_len(8).unwrap();
    assert_eq!(buf.payload(), &vec![0; 8]);
}
