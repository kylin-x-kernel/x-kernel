// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `sigsetsize` is only meaningful next to a real sigmask pointer.
//!
//! `pselect6` passes its mask through an argument pack; Linux feeds that pack's
//! inner pointer and size to `set_user_sigmask()`, which returns early for a
//! NULL mask (fs/select.c). A pack whose inner mask pointer is NULL must be
//! accepted whatever size it carries, while a real mask keeps the size rule.

use core::mem::size_of;

use kerrno::KError;
use linux_raw_sys::general::timespec;
use posix_types::{SignalSetWithSize, UserConstPtr, UserPtr, k_sigset};
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserBuffer, TestUserValue};

use super::sys_pselect6;

/// A zeroed `timespec` keeps the wait non-blocking.
fn zero_timeout() -> TestUserBuffer {
    let timeout = TestUserBuffer::new(size_of::<timespec>()).unwrap();
    timeout.write_bytes(&[0; size_of::<timespec>()]).unwrap();
    timeout
}

/// Builds the `pselect6` argument pack (`set` pointer followed by `sigsetsize`)
/// byte by byte: the struct fields are private, so its `repr(C)` layout is the
/// only construction path available to a test.
fn argpack(set: usize, sigsetsize: usize) -> TestUserBuffer {
    let pack = TestUserBuffer::new(size_of::<SignalSetWithSize>()).unwrap();
    let mut bytes = [0u8; size_of::<SignalSetWithSize>()];
    let width = size_of::<usize>();
    bytes[..width].copy_from_slice(&set.to_ne_bytes());
    bytes[width..].copy_from_slice(&sigsetsize.to_ne_bytes());
    pack.write_bytes(&bytes).unwrap();
    pack
}

#[def_test(user)]
fn test_pselect6_null_mask_accepts_any_sigsetsize() {
    let timeout = zero_timeout();

    for sigsetsize in [0, size_of::<k_sigset>(), 64, 128, 0xdead_beef] {
        let pack = argpack(0, sigsetsize);
        assert_eq!(
            sys_pselect6(
                0,
                UserPtr::default(),
                UserPtr::default(),
                UserPtr::default(),
                UserConstPtr::from(UserPtr::from(timeout.as_user_ptr::<timespec>())),
                UserConstPtr::from(UserPtr::from(pack.as_user_ptr::<SignalSetWithSize>())),
            ),
            Ok(0)
        );
    }
}

#[def_test(user)]
fn test_pselect6_with_mask_rejects_foreign_sigsetsize() {
    let timeout = zero_timeout();
    let sigmask = TestUserValue::from_value(k_sigset(0)).unwrap();
    let pack = argpack(sigmask.as_user_ptr() as usize, 64);

    assert_eq!(
        sys_pselect6(
            0,
            UserPtr::default(),
            UserPtr::default(),
            UserPtr::default(),
            UserConstPtr::from(UserPtr::from(timeout.as_user_ptr::<timespec>())),
            UserConstPtr::from(UserPtr::from(pack.as_user_ptr::<SignalSetWithSize>())),
        ),
        Err(KError::InvalidInput)
    );
}