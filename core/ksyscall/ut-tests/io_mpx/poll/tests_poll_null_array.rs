// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A NULL descriptor array with a zero count is legal for `poll`/`ppoll`.
//!
//! Linux copies the array only when `nfds > 0` (`len = min(nfds,
//! N_STACK_PPS)` in `do_sys_poll`), so `poll(NULL, 0, ...)` and
//! `ppoll(NULL, 0, ...)` return without touching the pointer. Reading through
//! it first would turn a legal call into `EFAULT`.

use core::mem::size_of;

use linux_raw_sys::general::timespec;
use posix_types::{UserConstPtr, UserPtr};
use unittest::{assert_eq, def_test};
use unittest_support::TestUserBuffer;

use super::sys_ppoll;
#[cfg(target_arch = "x86_64")]
use super::sys_poll;

/// A zeroed `timespec` keeps the wait non-blocking.
fn zero_timeout() -> TestUserBuffer {
    let timeout = TestUserBuffer::new(size_of::<timespec>()).unwrap();
    timeout.write_bytes(&[0; size_of::<timespec>()]).unwrap();
    timeout
}

#[def_test(user)]
fn test_ppoll_accepts_null_descriptor_array_with_zero_count() {
    let timeout = zero_timeout();

    assert_eq!(
        sys_ppoll(
            UserPtr::default(),
            0,
            UserConstPtr::from(UserPtr::from(timeout.as_user_ptr::<timespec>())),
            UserConstPtr::default(),
            0,
        ),
        Ok(0)
    );
}

#[cfg(target_arch = "x86_64")]
#[def_test(user)]
fn test_poll_accepts_null_descriptor_array_with_zero_count() {
    assert_eq!(sys_poll(UserPtr::default(), 0, 0), Ok(0));
}