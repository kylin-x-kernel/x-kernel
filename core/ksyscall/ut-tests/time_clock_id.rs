// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use posix_types::{UserConstPtr, UserPtr};
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn supported_clock_ids_keep_their_signed_abi_identity() {
    for (raw, expected) in [
        (CLOCK_REALTIME as i32, ClockId::Realtime),
        (CLOCK_MONOTONIC as i32, ClockId::Monotonic),
        (CLOCK_PROCESS_CPUTIME_ID as i32, ClockId::ProcessCpu),
        (CLOCK_THREAD_CPUTIME_ID as i32, ClockId::ThreadCpu),
        (CLOCK_MONOTONIC_RAW as i32, ClockId::MonotonicRaw),
        (CLOCK_REALTIME_COARSE as i32, ClockId::RealtimeCoarse),
        (CLOCK_MONOTONIC_COARSE as i32, ClockId::MonotonicCoarse),
        (CLOCK_BOOTTIME as i32, ClockId::Boottime),
    ] {
        assert_eq!(parse_clock_id(raw), Some(expected));
        assert_eq!(super::super::sys_clock_getres(raw, UserPtr::default()), Ok(0));
    }
}

#[def_test]
fn negative_fd_and_unknown_clock_ids_fail_before_user_memory_access() {
    // CLOCKFD uses low bits 3; -29 encodes fd 3. Dynamic clocks are not
    // implemented, and must remain rejected instead of wrapping to unsigned IDs.
    for raw in [i32::MIN, -29, -1, 8, 9, 11, i32::MAX] {
        assert_eq!(parse_clock_id(raw), None);
        assert_eq!(
            super::super::sys_clock_gettime(raw, UserPtr::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::super::sys_clock_getres(raw, UserPtr::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::super::sys_clock_settime(raw, UserConstPtr::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::super::sys_clock_nanosleep(raw, 0, UserConstPtr::default(), UserPtr::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::super::sys_timerfd_create(raw, 0),
            Err(KError::InvalidInput)
        );
    }
}

#[def_test]
fn operation_specific_clock_restrictions_keep_existing_errors() {
    for (raw, clock) in [
        (CLOCK_PROCESS_CPUTIME_ID as i32, ClockId::ProcessCpu),
        (CLOCK_THREAD_CPUTIME_ID as i32, ClockId::ThreadCpu),
        (CLOCK_MONOTONIC_RAW as i32, ClockId::MonotonicRaw),
        (CLOCK_REALTIME_COARSE as i32, ClockId::RealtimeCoarse),
        (CLOCK_MONOTONIC_COARSE as i32, ClockId::MonotonicCoarse),
    ] {
        let sleep_error = if clock == ClockId::MonotonicRaw {
            KError::OperationNotSupported
        } else {
            KError::InvalidInput
        };
        assert_eq!(timerfd_clock(clock), None);
        assert_eq!(
            super::super::sys_clock_nanosleep(raw, 0, UserConstPtr::default(), UserPtr::default()),
            Err(sleep_error)
        );
        assert_eq!(
            super::super::sys_timerfd_create(raw, 0),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::super::sys_clock_settime(raw, UserConstPtr::default()),
            Err(KError::InvalidInput)
        );
    }
    for (clock, expected) in [
        (ClockId::Realtime, TimerFdClock::Realtime),
        (ClockId::Monotonic, TimerFdClock::Monotonic),
        (ClockId::Boottime, TimerFdClock::Boottime),
    ] {
        assert_eq!(timerfd_clock(clock), Some(expected));
    }
}
