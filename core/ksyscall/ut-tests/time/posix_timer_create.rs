// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real user-copy and owner-lifetime regressions for POSIX timer creation.

use kerrno::KError;
use linux_raw_sys::general::{CLOCK_MONOTONIC, itimerspec, timespec};
use posix_types::{UserConstPtr, UserPtr, k_sigevent};
use unittest::{TestResult, assert, assert_eq, def_test};
use unittest_support::TestUserValue;

use super::{sys_timer_create, sys_timer_delete, sys_timer_gettime};

const EMPTY_SETTING: itimerspec = itimerspec {
    it_interval: timespec {
        tv_sec: 0,
        tv_nsec: 0,
    },
    it_value: timespec {
        tv_sec: 0,
        tv_nsec: 0,
    },
};

// The user executor constructs a fresh process. Its current owner starts with
// an empty timer map and ID 1; this is a fixture fact, not a Linux ID guarantee.
const FIRST_TIMER_ID: i32 = 1;

#[def_test(user, serial)]
fn timer_create_null_output_rolls_back_timer() -> TestResult {
    let setting = TestUserValue::from_value(EMPTY_SETTING).unwrap();
    let setting_ptr = UserPtr::from(setting.as_user_ptr());
    let outcome = (|| -> TestResult {
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_timer_create(
                CLOCK_MONOTONIC as i32,
                UserConstPtr::default(),
                UserPtr::default()
            ),
            Err(KError::BadAddress)
        );
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        TestResult::Ok
    })();
    // Clean up even when an assertion catches the original leaking behavior.
    let deleted = sys_timer_delete(FIRST_TIMER_ID);
    assert!(matches!(deleted, Ok(0) | Err(KError::InvalidInput)));
    outcome
}

#[def_test(user, serial)]
fn timer_create_unwritable_output_rolls_back_timer() -> TestResult {
    let setting = TestUserValue::from_value(EMPTY_SETTING).unwrap();
    let setting_ptr = UserPtr::from(setting.as_user_ptr());
    let bad_output = UserPtr::<i32>::from(1usize);
    assert!(matches!(
        bad_output.write_vm(-1).map_err(KError::from),
        Err(KError::BadAddress)
    ));
    let outcome = (|| -> TestResult {
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_timer_create(CLOCK_MONOTONIC as i32, UserConstPtr::default(), bad_output),
            Err(KError::BadAddress)
        );
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        TestResult::Ok
    })();
    let deleted = sys_timer_delete(FIRST_TIMER_ID);
    assert!(matches!(deleted, Ok(0) | Err(KError::InvalidInput)));
    outcome
}

#[def_test(user, serial)]
fn timer_create_success_publishes_unarmed_timer() -> TestResult {
    let timer_id = TestUserValue::from_value(-1i32).unwrap();
    let setting = TestUserValue::from_value(EMPTY_SETTING).unwrap();
    let setting_ptr = UserPtr::from(setting.as_user_ptr());
    let created = sys_timer_create(
        CLOCK_MONOTONIC as i32,
        UserConstPtr::default(),
        UserPtr::from(timer_id.as_user_ptr()),
    );
    let id = timer_id.read();
    let outcome = (|| -> TestResult {
        assert_eq!(created, Ok(0));
        assert_eq!(sys_timer_gettime(id, setting_ptr), Ok(0));
        let value = setting.read();
        assert_eq!(value.it_interval.tv_sec, 0);
        assert_eq!(value.it_interval.tv_nsec, 0);
        assert_eq!(value.it_value.tv_sec, 0);
        assert_eq!(value.it_value.tv_nsec, 0);
        TestResult::Ok
    })();
    // Assertions cannot skip removal of the successfully created object.
    let deleted = sys_timer_delete(id);
    if created == Ok(0) {
        assert_eq!(deleted, Ok(0));
        assert_eq!(
            sys_timer_gettime(id, setting_ptr),
            Err(KError::InvalidInput)
        );
    } else {
        assert_eq!(deleted, Err(KError::InvalidInput));
    }
    outcome
}

#[def_test(user, serial)]
fn timer_create_validation_order_preserves_empty_owner() -> TestResult {
    let setting = TestUserValue::from_value(EMPTY_SETTING).unwrap();
    let setting_ptr = UserPtr::from(setting.as_user_ptr());
    // The mapped buffer starts zeroed. The ABI POD permits every bit pattern.
    let mut event = TestUserValue::<k_sigevent>::new().unwrap();
    let mut invalid_event = event.read();
    invalid_event.sigev_notify = -1;
    event.write(invalid_event);
    let event_ptr = UserPtr::from(event.as_user_ptr()).as_const();
    let bad_output = UserPtr::<i32>::from(1usize);
    let bad_event = UserConstPtr::<k_sigevent>::from(1usize);
    let outcome = (|| -> TestResult {
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_timer_create(-1, UserConstPtr::default(), bad_output),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_timer_create(-1, bad_event, bad_output),
            Err(KError::BadAddress)
        );
        assert_eq!(
            sys_timer_create(CLOCK_MONOTONIC as i32, event_ptr, bad_output),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_timer_gettime(FIRST_TIMER_ID, setting_ptr),
            Err(KError::InvalidInput)
        );
        TestResult::Ok
    })();
    let deleted = sys_timer_delete(FIRST_TIMER_ID);
    assert!(matches!(deleted, Ok(0) | Err(KError::InvalidInput)));
    outcome
}
