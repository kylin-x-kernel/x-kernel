// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use unittest::{assert_eq, def_test};

use super::iov_iter_kvec_dest_rewind;

#[def_test]
fn rewind_restores_the_kernel_slice_cursor() {
    let mut bytes = [0u8; 4];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    assert_eq!(iter.copy_to_iter(&[1, 2, 3]), Ok(3));
    assert_eq!(iter.count(), 1);
    assert_eq!(iter.revert(3), Ok(()));
    assert_eq!(iter.count(), 4);
    assert_eq!(iter.copy_to_iter(&[9]), Ok(1));
    assert_eq!(bytes, [9, 2, 3, 0]);
}

#[def_test]
fn rewind_rejects_going_past_the_cursor_and_keeps_the_count() {
    let mut bytes = [0u8; 4];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    assert_eq!(iter.copy_to_iter(&[1, 2]), Ok(2));
    assert_eq!(iter.count(), 2);
    assert_eq!(iter.revert(3), Err(KError::InvalidInput));
    assert_eq!(iter.count(), 2);
}

#[def_test]
fn rewind_rejects_a_count_that_overflows_the_budget() {
    let mut bytes = [0u8; 4];
    let mut iter = iov_iter_kvec_dest_rewind(&mut bytes);
    assert_eq!(iter.revert(usize::MAX), Err(KError::InvalidInput));
    assert_eq!(iter.count(), 4);
}