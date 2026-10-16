// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Tests for resource-limit copying and atomic owner updates.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kerrno::KError;
use krlimit::Rlimit;
use ktask::WaitQueue;
use linux_raw_sys::general::{RLIM_NLIMITS, RLIMIT_CORE, RLIMIT_NOFILE, rlimit64};
use posix_types::{UserConstPtr, UserPtr};
use unittest::{TestResult, assert, assert_eq, def_test};
use unittest_support::TestUserValue;

#[def_test]
fn legacy_rlimit_syscalls_require_non_null_buffers() {
    assert_eq!(
        super::sys_getrlimit(0, UserPtr::default()),
        Err(KError::BadAddress)
    );
    assert_eq!(
        super::sys_setrlimit(0, UserConstPtr::default()),
        Err(KError::BadAddress)
    );
    // Linux's legacy getter and setter deliberately have different errno priority.
    assert_eq!(
        super::sys_getrlimit(RLIM_NLIMITS, UserPtr::default()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        super::sys_setrlimit(RLIM_NLIMITS, UserConstPtr::default()),
        Err(KError::BadAddress)
    );
}

#[def_test(user, serial)]
fn prlimit_aliased_buffer_updates_limit_and_returns_old_pair() -> TestResult {
    let original = TestUserValue::from_value(rlimit64 {
        rlim_cur: 0,
        rlim_max: 0,
    })
    .unwrap();
    assert_eq!(
        super::sys_prlimit64(
            0,
            RLIMIT_NOFILE,
            UserConstPtr::default(),
            UserPtr::from(original.as_user_ptr())
        ),
        Ok(0)
    );
    let old = original.read();
    assert!(old.rlim_cur > 0);
    let requested = old.rlim_cur - 1;
    // Allocate every fixture before changing the process limit.
    let alias = TestUserValue::from_value(rlimit64 {
        rlim_cur: requested,
        rlim_max: old.rlim_max,
    })
    .unwrap();
    let query = TestUserValue::from_value(rlimit64 {
        rlim_cur: 0,
        rlim_max: 0,
    })
    .unwrap();
    let alias_ptr = UserPtr::from(alias.as_user_ptr());
    // Framework assertions return from this closure; restoration still runs.
    let outcome = (|| -> TestResult {
        assert_eq!(
            super::sys_prlimit64(0, RLIMIT_NOFILE, alias_ptr.as_const(), alias_ptr),
            Ok(0)
        );
        let returned = alias.read();
        assert_eq!(returned.rlim_cur, old.rlim_cur);
        assert_eq!(returned.rlim_max, old.rlim_max);
        assert_eq!(
            super::sys_prlimit64(
                0,
                RLIMIT_NOFILE,
                UserConstPtr::default(),
                UserPtr::from(query.as_user_ptr())
            ),
            Ok(0)
        );
        let applied = query.read();
        assert_eq!(applied.rlim_cur, requested);
        assert_eq!(applied.rlim_max, old.rlim_max);
        TestResult::Ok
    })();
    // The hard limit is unchanged, so restoring the soft limit needs no elevation.
    let restored = super::sys_prlimit64(
        0,
        RLIMIT_NOFILE,
        UserPtr::from(original.as_user_ptr()).as_const(),
        UserPtr::default(),
    );
    assert_eq!(restored, Ok(0));
    outcome
}

#[def_test(user, serial)]
fn prlimit_rejected_update_preserves_aliased_buffer() {
    // cur > max is invalid without depending on privilege or current limits.
    let alias = TestUserValue::from_value(rlimit64 {
        rlim_cur: 2,
        rlim_max: 1,
    })
    .unwrap();
    let ptr = UserPtr::from(alias.as_user_ptr());
    assert_eq!(
        super::sys_prlimit64(0, RLIMIT_NOFILE, ptr.as_const(), ptr),
        Err(KError::InvalidInput)
    );
    let unchanged = alias.read();
    assert_eq!(unchanged.rlim_cur, 2);
    assert_eq!(unchanged.rlim_max, 1);
}

#[def_test(user, serial)]
fn prlimit_input_fault_precedes_pid_and_resource_errors() {
    // This fresh user task has no page-zero mapping. Prove the input faults.
    let invalid = UserConstPtr::<rlimit64>::from(1usize);
    assert!(matches!(
        invalid.read_vm().map_err(KError::from),
        Err(KError::BadAddress)
    ));
    assert_eq!(
        super::sys_prlimit64(
            kprocess::Pid::MAX,
            RLIMIT_NOFILE,
            invalid,
            UserPtr::default()
        ),
        Err(KError::BadAddress)
    );
    assert_eq!(
        super::sys_setrlimit(RLIM_NLIMITS, invalid),
        Err(KError::BadAddress)
    );
    assert_eq!(
        super::sys_prlimit64(0, RLIM_NLIMITS, UserConstPtr::default(), UserPtr::default()),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prlimit_output_fault_preserves_successful_update() -> TestResult {
    let original = TestUserValue::from_value(rlimit64 {
        rlim_cur: 0,
        rlim_max: 0,
    })
    .unwrap();
    assert_eq!(
        super::sys_prlimit64(
            0,
            RLIMIT_CORE,
            UserConstPtr::default(),
            UserPtr::from(original.as_user_ptr())
        ),
        Ok(0)
    );
    let old = original.read();
    assert!(old.rlim_max > 0);
    let requested = if old.rlim_cur == 0 {
        1
    } else {
        old.rlim_cur - 1
    };
    let input = TestUserValue::from_value(rlimit64 {
        rlim_cur: requested,
        rlim_max: old.rlim_max,
    })
    .unwrap();
    let query = TestUserValue::from_value(rlimit64 {
        rlim_cur: 0,
        rlim_max: 0,
    })
    .unwrap();
    // Prove this non-NULL output faults before changing the limit.
    let invalid = UserPtr::<rlimit64>::from(1usize);
    assert!(matches!(
        invalid
            .write_vm(rlimit64 {
                rlim_cur: 0,
                rlim_max: 0
            })
            .map_err(KError::from),
        Err(KError::BadAddress)
    ));
    let outcome = (|| -> TestResult {
        assert_eq!(
            super::sys_prlimit64(
                0,
                RLIMIT_CORE,
                UserPtr::from(input.as_user_ptr()).as_const(),
                invalid
            ),
            Err(KError::BadAddress)
        );
        assert_eq!(
            super::sys_prlimit64(
                0,
                RLIMIT_CORE,
                UserConstPtr::default(),
                UserPtr::from(query.as_user_ptr())
            ),
            Ok(0)
        );
        let applied = query.read();
        assert_eq!(applied.rlim_cur, requested);
        assert_eq!(applied.rlim_max, old.rlim_max);
        TestResult::Ok
    })();
    let restored = super::sys_prlimit64(
        0,
        RLIMIT_CORE,
        UserPtr::from(original.as_user_ptr()).as_const(),
        UserPtr::default(),
    );
    assert_eq!(restored, Ok(0));
    outcome
}

#[def_test(user, serial)]
fn concurrent_rlimit_exchanges_return_one_complete_transition_chain() {
    const WORKERS: usize = 8;

    let resources = kprocess::current_user_process()
        .resources()
        .expect("current process resources");
    let original = resources.rlimit(RLIMIT_CORE);
    assert!(original.is_ok());
    let original = original.unwrap();
    assert!(original.max >= WORKERS as u64);
    let returned = Arc::new(
        (0..WORKERS)
            .map(|_| (AtomicU64::new(0), AtomicU64::new(0), AtomicBool::new(false)))
            .collect::<Vec<_>>(),
    );
    let ready = Arc::new(AtomicUsize::new(0));
    let ready_queue = Arc::new(WaitQueue::new());
    let start = Arc::new(AtomicBool::new(false));
    let start_queue = Arc::new(WaitQueue::new());
    let mut workers = Vec::with_capacity(WORKERS);
    resources
        .set_rlimit(RLIMIT_CORE, Rlimit::new(0, original.max))
        .unwrap();

    for index in 0..WORKERS {
        let resources = Arc::clone(&resources);
        let returned = Arc::clone(&returned);
        let ready = Arc::clone(&ready);
        let ready_queue = Arc::clone(&ready_queue);
        let start = Arc::clone(&start);
        let start_queue = Arc::clone(&start_queue);
        let hard = original.max;
        workers.push(ktask::spawn(move || {
            // Both readiness and the start gate are outside the limits lock.
            ready.fetch_add(1, Ordering::Release);
            ready_queue.notify_one(false);
            start_queue.wait_until(|| start.load(Ordering::Acquire));
            let result = resources
                .update_rlimit_and_get_old(RLIMIT_CORE, Some(Rlimit::new(index as u64 + 1, hard)));
            if let Ok(old) = result {
                returned[index].0.store(old.current, Ordering::Relaxed);
                returned[index].1.store(old.max, Ordering::Relaxed);
                // Publish this worker's two independent observation slots.
                returned[index].2.store(true, Ordering::Release);
            }
        }));
    }
    ready_queue.wait_until(|| ready.load(Ordering::Acquire) == WORKERS);
    start.store(true, Ordering::Release);
    start_queue.notify_all(false);
    let exit_codes = workers
        .into_iter()
        .map(|worker| worker.join())
        .collect::<Vec<_>>();
    let final_limit = resources.update_rlimit_and_get_old(RLIMIT_CORE, None);
    // Reap every worker and restore the fixed-hard-limit state before assertions.
    let restored = resources.set_rlimit(RLIMIT_CORE, original);
    assert_eq!(restored, Ok(()));
    assert!(exit_codes.iter().all(|code| *code == 0));
    assert!(final_limit.is_ok());
    let final_limit = final_limit.unwrap();
    assert_eq!(final_limit.max, original.max);

    let mut next = [None; WORKERS + 1];
    for (index, observation) in returned.iter().enumerate() {
        // No failed update may be filtered out into a shorter passing history.
        assert!(observation.2.load(Ordering::Acquire));
        let predecessor = observation.0.load(Ordering::Relaxed);
        assert_eq!(observation.1.load(Ordering::Relaxed), original.max);
        assert!(predecessor <= WORKERS as u64);
        let predecessor = predecessor as usize;
        assert!(next[predecessor].is_none());
        next[predecessor] = Some(index + 1);
    }
    // Commit order is reconstructed from returned values, never return timing.
    let mut current = 0;
    for _ in 0..WORKERS {
        let successor = next[current].take();
        assert!(successor.is_some());
        current = successor.unwrap();
    }
    assert_eq!(current as u64, final_limit.current);
    assert!(next.iter().all(Option::is_none));
}
