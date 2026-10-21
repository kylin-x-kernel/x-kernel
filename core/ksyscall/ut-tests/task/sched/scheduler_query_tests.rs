// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KError;
use kprocess::AsThread;
use kspin::SpinNoPreempt;
use ktask::{TaskInner, current};
use linux_raw_sys::general::{SCHED_FIFO, SCHED_NORMAL};
use posix_types::{UserConstPtr, UserPtr};
use unittest::{TestResult, assert, assert_eq, def_test};
use unittest_support::TestUserValue;

use super::{
    SchedParam, scheduler_target, sys_sched_getparam, sys_sched_getscheduler,
    sys_sched_setparam, sys_sched_setscheduler,
};

fn run_worker(check_fn: fn() -> TestResult) -> TestResult {
    let prepared = kprocess::current_user_thread()
        .prepare_thread_clone()
        .expect("prepare scheduler query worker");
    let page_table_root = prepared.page_table_root();
    let (thread, task_number) = prepared.into_parts();
    let result = Arc::new(SpinNoPreempt::new(None));
    let worker_result = result.clone();
    let mut task = TaskInner::new_user(
        move || {
            let outcome = check_fn();
            *worker_result.lock() = Some(outcome);
            // Remove this fixture's published TID before its task exits.
            posix_process::do_exit(0, false);
        },
        "scheduler-query-worker".into(),
        task_number,
        thread,
    );
    task.ctx_mut().set_page_table_root(page_table_root);
    task.set_cpumask(current().cpumask());
    let worker = kprocess::start_user_task(task);
    worker.join();
    let outcome = result.lock().take().unwrap_or(TestResult::Failed);
    drop(worker);
    outcome
}

fn check_worker_queries() -> TestResult {
    let caller = current();
    let thread = caller.as_thread();
    let tid = thread.tid() as i32;
    assert!(tid != thread.pid() as i32, "fixture must be a non-leader");
    // Distinct fixture metadata makes a wrong representative observable.
    thread.set_scheduler(SCHED_FIFO, 7);
    let mut output = TestUserValue::from_value(SchedParam { sched_priority: -1 }).unwrap();
    for pid in [0, tid] {
        assert_eq!(sys_sched_getscheduler(pid), Ok(SCHED_FIFO as isize));
        output.write(SchedParam { sched_priority: -1 });
        assert_eq!(
            sys_sched_getparam(pid, UserPtr::from(output.as_user_ptr()).cast()),
            Ok(0)
        );
        assert_eq!(output.read().sched_priority, 7);
    }
    TestResult::Ok
}

fn check_worker_setter_thread_targets() -> TestResult {
    let caller = current();
    let thread = caller.as_thread();
    let pid = thread.pid() as i32;
    let tid = thread.tid() as i32;
    assert!(tid != pid, "fixture must be a non-leader");
    let leader = scheduler_target(pid).expect("published process leader");
    assert!(!caller.ptr_eq(&leader));
    assert_eq!(leader.as_thread().tid() as i32, pid);
    let leader_policy = leader.as_thread().scheduler_policy();
    let leader_priority = leader.as_thread().scheduler_priority();
    let mut param = TestUserValue::from_value(SchedParam { sched_priority: 7 }).unwrap();
    for target in [0, tid] {
        let resolved = scheduler_target(target).expect("calling worker target");
        assert!(caller.ptr_eq(&resolved));
        assert_eq!(
            sys_sched_setscheduler(
                target,
                SCHED_FIFO as i32,
                UserConstPtr::from(param.as_user_ptr().addr()),
            ),
            Ok(0)
        );
        assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
        param.write(SchedParam { sched_priority: 8 });
        assert_eq!(
            sys_sched_setparam(target, UserConstPtr::from(param.as_user_ptr().addr())),
            Ok(0)
        );
        assert_eq!(thread.scheduler_priority(), 8);
        assert_eq!(leader.as_thread().scheduler_policy(), leader_policy);
        assert_eq!(leader.as_thread().scheduler_priority(), leader_priority);
        param.write(SchedParam { sched_priority: 7 });
    }
    // Reapplying the leader's own parameter must not modify this worker.
    param.write(SchedParam {
        sched_priority: leader_priority,
    });
    assert_eq!(
        sys_sched_setparam(pid, UserConstPtr::from(param.as_user_ptr().addr())),
        Ok(0)
    );
    assert_eq!(thread.scheduler_priority(), 8);
    assert_eq!(leader.as_thread().scheduler_priority(), leader_priority);
    TestResult::Ok
}

#[def_test(user, serial)]
fn scheduler_queries_reject_negative_and_missing_ids() {
    for pid in [-1, i32::MIN] {
        assert_eq!(sys_sched_getscheduler(pid), Err(KError::InvalidInput));
        assert_eq!(
            sys_sched_getparam(pid, UserPtr::default()),
            Err(KError::InvalidInput)
        );
    }
    assert_eq!(sys_sched_getscheduler(i32::MAX), Err(KError::NoSuchProcess));
    assert_eq!(
        sys_sched_getparam(i32::MAX, UserPtr::default()),
        Err(KError::NoSuchProcess)
    );
}

fn getaffinity_raw(pid: i32, size_register: usize, pointer: usize) -> isize {
    let mut context = khal::uspace::UserContext::new(0, 0x8000usize.into(), 0);
    context.set_sysno(linux_sysno::Sysno::sched_getaffinity.id() as usize);
    context.set_arg0(pid as usize);
    context.set_arg1(size_register);
    context.set_arg2(pointer);
    core::assert_eq!(
        crate::handle_syscall(&mut context),
        kprocess::UserThreadRuntimeAction::Continue
    );
    context.retval() as isize
}

#[def_test(user, serial)]
fn scheduler_getaffinity_obeys_native_word_abi() {
    let mut output = TestUserValue::from_value([0xa5u8; 24]).unwrap();
    let pointer = output.as_user_ptr().addr() + 8;
    for pid in [0, i32::MAX] {
        for size in [0, 1, 7, 9, 15] {
            assert_eq!(getaffinity_raw(pid, size, pointer), -22);
            assert_eq!(output.read(), [0xa5; 24]);
        }
    }
    assert_eq!(getaffinity_raw(i32::MAX, 1, 0), -22);
    assert_eq!(getaffinity_raw(i32::MAX, 8, 0), -3);
    assert_eq!(getaffinity_raw(0, 8, 0), -14);
    let mut expected = 0u64;
    for cpu in 0..kbuild_config::NR_CPUS {
        if current().cpumask().get(cpu) {
            expected |= 1u64 << cpu;
        }
    }
    let expected_bytes = expected.to_ne_bytes();
    // Length alignment applies to len, not to the user buffer address.
    for offset in [1, 8] {
        let pointer = output.as_user_ptr().addr() + offset;
        for size in [8, 16, (u32::MAX as usize) & !7] {
            output.write([0xa5; 24]);
            assert_eq!(getaffinity_raw(0, size, pointer), 8);
            let bytes = output.read();
            assert_eq!(&bytes[..offset], &[0xa5; 24][..offset]);
            assert_eq!(&bytes[offset..offset + 8], expected_bytes.as_slice());
            assert_eq!(&bytes[offset + 8..], &[0xa5; 24][offset + 8..]);
        }
    }
}

#[def_test(user, serial)]
fn scheduler_getaffinity_truncates_len_to_u32_at_dispatch() {
    let mut output = TestUserValue::from_value([0xa5u8; 24]).unwrap();
    let pointer = output.as_user_ptr().addr() + 8;
    for raw in [1usize << 61, usize::MAX, (1usize << 32) | 1] {
        assert_eq!(getaffinity_raw(0, raw, pointer), -22);
        assert_eq!(output.read(), [0xa5; 24]);
    }
    for raw in [(1usize << 32) | 8, (1usize << 61) | 8] {
        output.write([0xa5; 24]);
        assert_eq!(getaffinity_raw(0, raw, pointer), 8);
        let bytes = output.read();
        assert_eq!(&bytes[..8], &[0xa5; 8]);
        assert_eq!(&bytes[16..], &[0xa5; 8]);
        for byte in &bytes[9..16] {
            assert_eq!(*byte, 0);
        }
    }
}

#[def_test(user, serial)]
fn scheduler_worker_queries_resolve_zero_and_current_tid() -> TestResult {
    run_worker(check_worker_queries)
}

#[def_test(user, serial)]
fn scheduler_query_and_setters_address_published_worker_tid() -> TestResult {
    let prepared = kprocess::current_user_thread()
        .prepare_thread_clone()
        .expect("prepare published scheduler query worker");
    let tid = prepared.tid() as i32;
    let page_table_root = prepared.page_table_root();
    let (thread, task_number) = prepared.into_parts();
    thread.set_scheduler(SCHED_FIFO, 7);
    let mut task = TaskInner::new_user(
        || {},
        "scheduler-inactive-worker".into(),
        task_number,
        thread,
    );
    task.ctx_mut().set_page_table_root(page_table_root);
    let published = kprocess::publish_user_task(task).unwrap();
    // This owned publication stays live deterministically until abort;
    // assertion returns still drop the handle and roll back publication.
    let outcome = (|| -> TestResult {
        assert!(tid != kprocess::current_user_thread().pid() as i32);
        let param = TestUserValue::from_value(SchedParam { sched_priority: 0 }).unwrap();
        let output = TestUserValue::from_value(SchedParam { sched_priority: -1 }).unwrap();
        assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
        assert_eq!(
            sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
            Ok(0)
        );
        assert_eq!(output.read().sched_priority, 7);
        // The published worker stays root-owned while the caller drops
        // its credentials; both actual setters must deny cross-owner writes.
        let caller = kprocess::current_user_thread();
        let saved_cred = caller.cred();
        let denied = (|| -> TestResult {
            // unittest_support starts each user test with initial_cred();
            // fail explicitly if that root-owned fixture contract changes.
            assert_eq!(saved_cred.ruid(), 0);
            assert_eq!(saved_cred.euid(), 0);
            assert_eq!(crate::task::credentials::sys_setuid(3000), Ok(0));
            assert_eq!(caller.cred().euid(), 3000);
            assert_eq!(published.task().as_thread().pid(), caller.pid());
            assert_eq!(
                published.task().as_thread().real_cred().ruid(),
                saved_cred.ruid()
            );
            assert_eq!(
                published.task().as_thread().real_cred().euid(),
                saved_cred.euid()
            );
            assert_eq!(
                sys_sched_setscheduler(
                    tid,
                    SCHED_NORMAL as i32,
                    UserConstPtr::from(param.as_user_ptr().addr())
                ),
                Err(KError::OperationNotPermitted)
            );
            let rt_param = TestUserValue::from_value(SchedParam { sched_priority: 8 }).unwrap();
            assert_eq!(
                sys_sched_setparam(tid, UserConstPtr::from(rt_param.as_user_ptr().addr())),
                Err(KError::OperationNotPermitted)
            );
            assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_FIFO as isize));
            assert_eq!(
                sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
                Ok(0)
            );
            assert_eq!(output.read().sched_priority, 7);
            TestResult::Ok
        })();
        // Restore credentials even when an inner assertion returns failure.
        caller.commit_creds((*saved_cred).clone());
        assert_eq!(denied, TestResult::Ok);
        for pid in [-1, i32::MIN, i32::MAX] {
            assert_eq!(
                sys_sched_setscheduler(
                    pid,
                    SCHED_NORMAL as i32,
                    UserConstPtr::from(param.as_user_ptr().addr()),
                ),
                Err(KError::NoSuchProcess)
            );
            assert_eq!(
                sys_sched_setparam(pid, UserConstPtr::from(param.as_user_ptr().addr())),
                Err(KError::NoSuchProcess)
            );
        }
        assert_eq!(
            sys_sched_setscheduler(
                tid,
                SCHED_NORMAL as i32,
                UserConstPtr::from(param.as_user_ptr().addr()),
            ),
            Ok(0)
        );
        assert_eq!(
            sys_sched_setparam(tid, UserConstPtr::from(param.as_user_ptr().addr())),
            Ok(0)
        );
        assert_eq!(sys_sched_getscheduler(tid), Ok(SCHED_NORMAL as isize));
        assert_eq!(
            sys_sched_getparam(tid, UserPtr::from(output.as_user_ptr()).cast()),
            Ok(0)
        );
        assert_eq!(output.read().sched_priority, 0);
        TestResult::Ok
    })();
    published.abort();
    // An unpublished worker TID must no longer be a read or write target.
    assert_eq!(sys_sched_getscheduler(tid), Err(KError::NoSuchProcess));
    assert_eq!(
        sys_sched_setscheduler(tid, SCHED_NORMAL as i32, UserConstPtr::default()),
        Err(KError::NoSuchProcess)
    );
    assert_eq!(
        sys_sched_setparam(tid, UserConstPtr::default()),
        Err(KError::NoSuchProcess)
    );
    assert_eq!(
        sys_sched_getparam(tid, UserPtr::default()),
        Err(KError::NoSuchProcess)
    );
    outcome
}

#[def_test(user, serial)]
fn scheduler_setters_resolve_worker_and_leader_tids() -> TestResult {
    run_worker(check_worker_setter_thread_targets)
}
