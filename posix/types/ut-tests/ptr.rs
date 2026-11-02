// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn owner_failure_precedes_user_access() {
    let attempted = Cell::new(None);
    let result = UserConstPtr::<u8>::from(0usize).load_vm_kbox_with(
        8,
        AllocFlags::NOWAIT,
        |len, flags| {
            attempted.set(Some((len, flags)));
            Err(KError::NoMemory)
        },
    );
    assert_eq!(attempted.get(), Some((8, AllocFlags::NOWAIT)));
    assert_eq!(result.err(), Some(KError::NoMemory));
}

#[def_test]
fn empty_overflow_and_invalid_flags() {
    let pointer = UserConstPtr::<u8>::from(0usize);
    assert!(pointer.load_vm_kbox(0).unwrap().is_empty());
    assert_eq!(pointer.load_vm_kbox(usize::MAX).err(), Some(KError::InvalidInput));
    assert_eq!(
        pointer.load_vm_kbox_with_flags(0, AllocFlags::from_bits_retain(u32::MAX)).err(),
        Some(KError::InvalidInput),
    );
}

#[def_test]
fn reservation_length_mismatch_is_rejected() {
    let result = UserConstPtr::<u8>::from(0usize).load_vm_kbox_with(
        8,
        AllocFlags::NOWAIT,
        |_, flags| KBox::new_uninit_slice_with_flags(0, flags),
    );
    assert_eq!(result.err(), Some(KError::InvalidInput));
}
