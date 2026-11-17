// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use linux_raw_sys::general::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME_COARSE, CLOCK_THREAD_CPUTIME_ID,
    timespec,
};
use posix_types::{UserConstPtr, UserPtr};
use unittest::{assert, assert_eq, def_test};
use unittest_support::TestUserValue;

#[def_test]
fn gettimeofday_accepts_null_timeval() {
    assert_eq!(super::sys_gettimeofday(UserPtr::default()), Ok(0));
}

#[def_test(user)]
fn scheduler_cpu_clock_resolution_is_one_nanosecond() {
    let output = TestUserValue::from_value(timespec { tv_sec: 0, tv_nsec: 0 }).unwrap();
    for clock in [CLOCK_PROCESS_CPUTIME_ID as i32, CLOCK_THREAD_CPUTIME_ID as i32] {
        assert_eq!(super::sys_clock_getres(clock, UserPtr::from(output.as_user_ptr())), Ok(0));
        let resolution = output.read();
        assert_eq!(resolution.tv_sec, 0);
        assert_eq!(resolution.tv_nsec, 1);
    }
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
