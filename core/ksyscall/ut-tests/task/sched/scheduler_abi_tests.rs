// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use ktask::DEFAULT_RR_SLICE_NS;
use linux_raw_sys::general::{
    SCHED_DEADLINE, SCHED_FIFO, SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, timespec,
};
use posix_types::{UserConstPtr, UserPtr};
use unittest::{TestResult, assert_eq, def_test};
use unittest_support::TestUserValue;

use super::{
    SchedParam, sys_sched_get_priority_max, sys_sched_get_priority_min, sys_sched_getattr,
    sys_sched_getparam, sys_sched_getscheduler, sys_sched_rr_get_interval, sys_sched_setattr,
    sys_sched_setparam, sys_sched_setscheduler,
};

fn sched_param(priority: i32) -> TestUserValue<SchedParam> {
    TestUserValue::from_value(SchedParam {
        sched_priority: priority,
    })
    .expect("map sched_param")
}

fn restore_normal() {
    let param = sched_param(0);
    let _ = sys_sched_setscheduler(
        0,
        SCHED_NORMAL as i32,
        UserConstPtr::from(param.as_user_ptr().addr()),
    );
}

#[def_test(user, serial)]
fn scheduler_priority_bounds_follow_linux() {
    assert_eq!(sys_sched_get_priority_min(SCHED_FIFO as i32), Ok(1));
    assert_eq!(sys_sched_get_priority_max(SCHED_RR as i32), Ok(99));
    assert_eq!(sys_sched_get_priority_min(SCHED_NORMAL as i32), Ok(0));
    assert_eq!(sys_sched_get_priority_max(SCHED_DEADLINE as i32), Ok(0));
    assert_eq!(sys_sched_get_priority_min(-1), Err(KError::InvalidInput));
    assert_eq!(sys_sched_get_priority_max(4), Err(KError::InvalidInput));
}

#[def_test(user, serial)]
fn scheduler_rr_interval_is_finite_only_for_rr() -> TestResult {
    let outcome = (|| -> TestResult {
        let fifo = sched_param(1);
        assert_eq!(
            sys_sched_setscheduler(
                0,
                SCHED_FIFO as i32,
                UserConstPtr::from(fifo.as_user_ptr().addr()),
            ),
            Ok(0)
        );
        let interval = TestUserValue::from_value(timespec {
            tv_sec: 1,
            tv_nsec: 1,
        })
        .expect("map timespec");
        assert_eq!(
            sys_sched_rr_get_interval(0, UserPtr::from(interval.as_user_ptr())),
            Ok(0)
        );
        assert_eq!(interval.read().tv_sec, 0);
        assert_eq!(interval.read().tv_nsec, 0);

        let rr = sched_param(1);
        assert_eq!(
            sys_sched_setscheduler(
                0,
                SCHED_RR as i32,
                UserConstPtr::from(rr.as_user_ptr().addr()),
            ),
            Ok(0)
        );
        assert_eq!(
            sys_sched_rr_get_interval(0, UserPtr::from(interval.as_user_ptr())),
            Ok(0)
        );
        let written = interval.read();
        let nanos = (written.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(written.tv_nsec as u64);
        assert_eq!(nanos, DEFAULT_RR_SLICE_NS);
        assert_eq!(sys_sched_getscheduler(0), Ok(SCHED_RR as isize));
        TestResult::Ok
    })();
    restore_normal();
    assert_eq!(sys_sched_getscheduler(0), Ok(SCHED_NORMAL as isize));
    let param = TestUserValue::from_value(SchedParam { sched_priority: -1 }).unwrap();
    assert_eq!(
        sys_sched_getparam(0, UserPtr::from(param.as_user_ptr()).cast()),
        Ok(0)
    );
    assert_eq!(param.read().sched_priority, 0);
    outcome
}

#[def_test(user, serial)]
fn scheduler_rejects_null_param_before_lookup() {
    assert_eq!(
        sys_sched_setparam(-1, UserConstPtr::default()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_sched_setscheduler(0, SCHED_FIFO as i32, UserConstPtr::default()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_sched_rr_get_interval(-1, UserPtr::default()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_sched_rr_get_interval(0, UserPtr::default()),
        Err(KError::BadAddress)
    );
}

fn fair_sched_attr(nice: i32) -> TestUserValue<[u8; 48]> {
    let mut bytes = [0u8; 48];
    bytes[0..4].copy_from_slice(&48u32.to_ne_bytes());
    bytes[4..8].copy_from_slice(&SCHED_NORMAL.to_ne_bytes());
    bytes[16..20].copy_from_slice(&nice.to_ne_bytes());
    TestUserValue::from_value(bytes).expect("map sched_attr")
}

fn current_sched_nice() -> Result<i32, KError> {
    let buf = TestUserValue::from_value([0u8; 56]).expect("map sched_attr out");
    sys_sched_getattr(0, UserPtr::from(buf.as_user_ptr().addr()), 56, 0)?;
    let bytes = buf.read();
    Ok(i32::from_ne_bytes(bytes[16..20].try_into().unwrap()))
}

fn restore_normal_nice() {
    restore_normal();
    let attr = fair_sched_attr(0);
    let _ = sys_sched_setattr(0, UserConstPtr::from(attr.as_user_ptr().addr()), 0);
}

#[def_test(user, serial)]
fn reset_on_fork_clear_requires_privilege() -> TestResult {
    let param = sched_param(0);
    let param_ptr = UserConstPtr::from(param.as_user_ptr().addr());
    let with_flag = (SCHED_NORMAL | SCHED_RESET_ON_FORK) as i32;
    let outcome = (|| -> TestResult {
        assert_eq!(sys_sched_setscheduler(0, with_flag, param_ptr), Ok(0));
        assert_eq!(
            super::super::credentials::sys_setresuid(u32::MAX, 1000, u32::MAX),
            Ok(0)
        );
        assert_eq!(
            sys_sched_setscheduler(0, SCHED_NORMAL as i32, param_ptr),
            Err(KError::OperationNotPermitted)
        );
        assert_eq!(sys_sched_getscheduler(0), Ok(with_flag as isize));
        TestResult::Ok
    })();
    let _ = super::super::credentials::sys_setuid(0);
    assert_eq!(
        sys_sched_setscheduler(0, SCHED_NORMAL as i32, param_ptr),
        Ok(0)
    );
    assert_eq!(sys_sched_getscheduler(0), Ok(SCHED_NORMAL as isize));
    outcome
}

#[def_test(user, serial)]
fn sched_setattr_clamps_nice_and_hides_it_for_realtime() -> TestResult {
    let outcome = (|| -> TestResult {
        let high = fair_sched_attr(1000);
        assert_eq!(
            sys_sched_setattr(0, UserConstPtr::from(high.as_user_ptr().addr()), 0),
            Ok(0)
        );
        assert_eq!(current_sched_nice(), Ok(19));

        let fifo = sched_param(1);
        assert_eq!(
            sys_sched_setscheduler(
                0,
                SCHED_FIFO as i32,
                UserConstPtr::from(fifo.as_user_ptr().addr()),
            ),
            Ok(0)
        );
        assert_eq!(current_sched_nice(), Ok(0));
        restore_normal();
        assert_eq!(current_sched_nice(), Ok(19));

        let low = fair_sched_attr(-1000);
        assert_eq!(
            sys_sched_setattr(0, UserConstPtr::from(low.as_user_ptr().addr()), 0),
            Ok(0)
        );
        assert_eq!(current_sched_nice(), Ok(-20));
        TestResult::Ok
    })();
    restore_normal_nice();
    outcome
}

#[def_test(user, serial)]
fn sched_setattr_short_size_rewrites_e2big() {
    let mut bytes = [0u8; 48];
    bytes[0..4].copy_from_slice(&16u32.to_ne_bytes());
    let buf = TestUserValue::from_value(bytes).unwrap();
    assert_eq!(
        sys_sched_setattr(0, UserConstPtr::from(buf.as_user_ptr().addr()), 0),
        Err(KError::ArgumentListTooLong)
    );
    assert_eq!(u32::from_ne_bytes(buf.read()[0..4].try_into().unwrap()), 56);
}
