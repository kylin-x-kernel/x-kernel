// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use linux_raw_sys::general::{
    AT_FDCWD, AT_STATX_DONT_SYNC, AT_STATX_FORCE_SYNC, S_IFDIR, S_IFMT, STATX_BASIC_STATS,
};
use unittest::{assert_eq, def_test};
use unittest_support::{TestUserArray, TestUserValue};

use super::*;

#[def_test(user)]
fn invalid_statx_requests_preserve_output_and_precede_lookup() {
    let root = TestUserArray::from_array(*b"/\0").unwrap();
    let empty = TestUserArray::from_array(*b"\0").unwrap();
    let missing = TestUserArray::from_array(*b"/no-such-statx-probe-entry\0").unwrap();
    let output = TestUserArray::from_array([0xa5_u8; size_of::<statx>()]).unwrap();
    for path in [
        root.as_user_ptr(),
        empty.as_user_ptr(),
        missing.as_user_ptr(),
    ] {
        for (flags, mask) in [
            (0, STATX__RESERVED),
            (AT_STATX_SYNC_TYPE, STATX_BASIC_STATS),
        ] {
            for dirfd in [AT_FDCWD, -123] {
                assert_eq!(
                    sys_statx(
                        dirfd,
                        UserConstPtr::from(path.cast::<c_char>().cast_const()),
                        flags,
                        mask,
                        UserPtr::from(output.as_user_ptr().cast::<statx>()),
                    ),
                    Err(KError::InvalidInput)
                );
                assert_eq!(output.read(), [0xa5; size_of::<statx>()]);
            }
        }
    }
}

#[def_test(user)]
fn statx_request_errors_precede_user_buffer_access() {
    // Linux do_statx checks these request bits before propagating getname errors.
    for path in [0_usize, 1] {
        for (flags, mask) in [(0, STATX__RESERVED), (AT_STATX_SYNC_TYPE, 0)] {
            assert_eq!(
                sys_statx(
                    AT_FDCWD,
                    UserConstPtr::from(path),
                    flags,
                    mask,
                    UserPtr::default(),
                ),
                Err(KError::InvalidInput)
            );
        }
    }
    let empty = TestUserArray::from_array(*b"\0").unwrap();
    // Without invalid request bits, an empty path still requires AT_EMPTY_PATH.
    assert_eq!(
        sys_statx(
            AT_FDCWD,
            UserConstPtr::from(empty.as_user_ptr().cast::<c_char>().cast_const()),
            0,
            STATX_BASIC_STATS,
            UserPtr::default(),
        ),
        Err(KError::NotFound)
    );
}

#[def_test(user)]
fn statx_accepts_mask_hints_and_individual_sync_modes() {
    let root = TestUserArray::from_array(*b"/\0").unwrap();
    let output = TestUserValue::<statx>::new().unwrap();
    for flags in [0, AT_STATX_FORCE_SYNC, AT_STATX_DONT_SYNC] {
        for mask in [0, STATX_BASIC_STATS, 0x4000_0000] {
            assert_eq!(
                sys_statx(
                    AT_FDCWD,
                    UserConstPtr::from(root.as_user_ptr().cast::<c_char>().cast_const()),
                    flags,
                    mask,
                    UserPtr::from(output.as_user_ptr()),
                ),
                Ok(0)
            );
            assert_eq!(u32::from(output.read().stx_mode) & S_IFMT, S_IFDIR);
        }
    }
}
