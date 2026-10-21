// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use linux_raw_sys::general::{CLOCK_MONOTONIC, CLOCK_REALTIME_COARSE, timespec};
use posix_types::{UserConstPtr, UserPtr};
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn gettimeofday_accepts_null_timeval() {
    assert_eq!(super::sys_gettimeofday(UserPtr::default()), Ok(0));
}

#[def_test(user, serial)]
fn clock_settime_rejects_query_only_clocks_before_reading_time() {
    // Address 1 is outside the valid user range. Prove BadAddress from
    // access validation; this does not exercise a page fault during copying.
    let invalid = UserConstPtr::<timespec>::from(1usize);
    assert!(matches!(
        invalid.read_vm().map_err(KError::from),
        Err(KError::BadAddress)
    ));
    for clock in [CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC] {
        assert_eq!(
            super::sys_clock_settime(clock as _, invalid),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::sys_clock_settime(clock as _, UserConstPtr::default()),
            Err(KError::InvalidInput)
        );
    }
}
