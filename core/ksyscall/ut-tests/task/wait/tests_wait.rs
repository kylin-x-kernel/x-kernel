// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::LinuxError;
use kprocess::{current_user_process, process_exit, wait_reap};
use ksignal::{SignalAction, SignalActionFlags, SignalDisposition, Signo};
use ktask::WaitQueue;
use unittest::{assert_eq, def_test};

use super::{
    WaitOptions, WaitPid, matching_children, wait_on_matching_children, wait_test_sync,
};

fn reap_test_child(
    parent: &alloc::sync::Arc<kprocess::Process>,
    child: &alloc::sync::Arc<kprocess::Process>,
) {
    if !child.is_waitable_zombie() {
        process_exit::finalize_process_exit(child);
    }
    wait_reap::assert_reap_zombie_process(child);
    assert!(
        !parent
            .children()
            .iter()
            .any(|proc| proc.pid() == child.pid()),
        "test child {} should be reaped from parent",
        child.pid()
    );
}

fn complete_sigchld_child_exit(
    parent: &alloc::sync::Arc<kprocess::Process>,
    child: &alloc::sync::Arc<kprocess::Process>,
) -> bool {
    debug_assert!(
        parent
            .children()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, child)),
        "test helper should only complete a live child exit"
    );
    process_exit::complete_process_exit(child)
}

#[def_test(user, serial)]
fn test_waitable_child_rechecks_after_new_child_appears() {
    let parent = current_user_process();
    let existing = parent.fork(8_100);
    let late = parent.fork(8_101);

    let children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
    assert_eq!(children.len(), 2);
    assert!(!children.iter().any(|child| child.is_waitable_zombie()));

    process_exit::finalize_process_exit(&late);

    let children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
    let waitable = children
        .iter()
        .find(|child| child.is_waitable_zombie())
        .cloned()
        .expect("wait must observe child that exits after initial scan");
    assert_eq!(waitable.pid(), late.pid());

    reap_test_child(&parent, &late);
    reap_test_child(&parent, &existing);
}

#[def_test(user, serial)]
fn test_matching_children_reflects_reaped_children() {
    let parent = current_user_process();
    let child = parent.fork(8_110);

    assert_eq!(
        matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).len(),
        1
    );

    reap_test_child(&parent, &child);

    assert!(
        matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
        "matching child set must be recomputed from the live parent/child relation"
    );
}

#[def_test(user, serial)]
fn test_wait_options_filter_clone_children_by_exit_signal() {
    let parent = current_user_process();
    let sigchld_child = parent.fork(8_115);
    let clone_child = parent.fork_with_exit_signal(8_116, Some(Signo::SIGUSR1));

    let default_children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
    assert!(
        default_children
            .iter()
            .any(|child| child.pid() == sigchld_child.pid())
    );
    assert!(
        default_children
            .iter()
            .all(|child| child.pid() != clone_child.pid())
    );

    let clone_children = matching_children(&parent, WaitPid::Any, WaitOptions::WCLONE);
    assert!(
        clone_children
            .iter()
            .any(|child| child.pid() == clone_child.pid())
    );
    assert!(
        clone_children
            .iter()
            .all(|child| child.pid() != sigchld_child.pid())
    );

    let all_children = matching_children(&parent, WaitPid::Any, WaitOptions::WALL);
    assert!(
        all_children
            .iter()
            .any(|child| child.pid() == sigchld_child.pid())
    );
    assert!(
        all_children
            .iter()
            .any(|child| child.pid() == clone_child.pid())
    );

    reap_test_child(&parent, &sigchld_child);
    reap_test_child(&parent, &clone_child);
}

#[def_test(user, serial)]
fn test_waitpid_prefers_ready_zombie_over_pending_interrupt() {
    let parent = current_user_process();
    let child = parent.fork(8_120);

    process_exit::finalize_process_exit(&child);
    ktask::current().interrupt();

    let waited = wait_on_matching_children(
        &parent,
        WaitPid::Pid(child.pid()),
        core::ptr::null_mut(),
        WaitOptions::empty(),
    )
    .expect("wait must reap an already exited child before returning ERESTARTSYS");

    ktask::current().clear_interrupt();
    assert_eq!(waited, child.pid() as isize);
    assert!(
        matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
        "reaped child must be removed from the live child set"
    );
}

#[def_test(user, serial)]
fn test_wnowait_reports_without_reaping_child() {
    let parent = current_user_process();
    let child = parent.fork(8_119);

    process_exit::finalize_process_exit(&child);

    let peeked = wait_on_matching_children(
        &parent,
        WaitPid::Pid(child.pid()),
        core::ptr::null_mut(),
        WaitOptions::WNOWAIT,
    )
    .expect("WNOWAIT must report a waitable zombie");

    assert_eq!(peeked, child.pid() as isize);
    assert!(
        matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty())
            .iter()
            .any(|candidate| candidate.is_waitable_zombie()),
        "WNOWAIT must leave the child waitable"
    );

    let reaped = wait_on_matching_children(
        &parent,
        WaitPid::Pid(child.pid()),
        core::ptr::null_mut(),
        WaitOptions::empty(),
    )
    .expect("a later wait must still be able to reap the child");

    assert_eq!(reaped, child.pid() as isize);
    assert!(
        matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
        "ordinary wait must consume the child after WNOWAIT"
    );
}

#[def_test(user, serial)]
fn test_waitpid_reports_restartsys_when_only_interrupt_is_ready() {
    let parent = current_user_process();
    let child = parent.fork(8_121);

    ktask::current().interrupt();
    let err = wait_on_matching_children(
        &parent,
        WaitPid::Pid(child.pid()),
        core::ptr::null_mut(),
        WaitOptions::empty(),
    )
    .expect_err("wait without a waitable child should still report interruption");
    ktask::current().clear_interrupt();

    assert_eq!(LinuxError::from(err), LinuxError::ERESTARTSYS);
    reap_test_child(&parent, &child);
}

#[def_test(user, serial)]
fn test_concurrent_waiters_must_rescan_after_register_before_reaping() {
    let parent = current_user_process();
    let child = parent.fork(8_122);
    let child_pid = child.pid();

    let reaped = Arc::new(AtomicUsize::new(0));
    let echld = Arc::new(AtomicUsize::new(0));
    let unexpected = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let finished_wq = Arc::new(WaitQueue::new());

    wait_test_sync::arm_register_barrier();

    let spawn_waiter = || {
        let parent = parent.clone();
        let reaped = reaped.clone();
        let echld = echld.clone();
        let unexpected = unexpected.clone();
        let finished = finished.clone();
        let finished_wq = finished_wq.clone();
        ktask::spawn(move || {
            match wait_on_matching_children(
                &parent,
                WaitPid::Pid(child_pid),
                core::ptr::null_mut(),
                WaitOptions::empty(),
            )
            .map_err(LinuxError::from)
            {
                Ok(pid) if pid == child_pid as isize => {
                    reaped.fetch_add(1, Ordering::AcqRel);
                }
                Err(LinuxError::ECHILD) => {
                    echld.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    unexpected.fetch_add(1, Ordering::AcqRel);
                }
            }
            finished.fetch_add(1, Ordering::AcqRel);
            finished_wq.notify_one(true);
        })
    };

    let waiter_a = spawn_waiter();
    let waiter_b = spawn_waiter();

    // Wait path checks for a ready child before registering. Keep the child
    // live until both waiters have registered, otherwise they return early
    // and never hit the barrier (hanging this test).
    while wait_test_sync::register_barrier_arrivals() < 2 {
        ktask::yield_now();
    }
    process_exit::finalize_process_exit(&child);
    wait_test_sync::release_register_barrier();

    finished_wq.wait_until(|| finished.load(Ordering::Acquire) == 2);
    wait_test_sync::disarm_register_barrier();

    assert_eq!(waiter_a.join(), 0);
    assert_eq!(waiter_b.join(), 0);

    assert_eq!(
        reaped.load(Ordering::Acquire),
        1,
        "exactly one waiter must win the reap race"
    );
    assert_eq!(
        echld.load(Ordering::Acquire),
        1,
        "the losing waiter must re-scan the live child set instead of reusing a stale snapshot"
    );
    assert_eq!(
        unexpected.load(Ordering::Acquire),
        0,
        "both waiters should resolve to either the reaped pid or ECHILD"
    );

    assert!(
        matching_children(&parent, WaitPid::Pid(child_pid), WaitOptions::empty()).is_empty(),
        "the child must be removed from the live child set after exactly one waiter reaps it"
    );
}

#[def_test(user, serial)]
fn test_sigchld_explicit_ignore_autoreap_wakes_waiter_with_echild() {
    let parent = current_user_process();
    let child = parent.fork(8_230);
    let child_pid = child.pid();
    let signals = parent
        .signal_manager()
        .expect("current process signal manager");
    let old_action = signals
        .replace_signal_action(
            Signo::SIGCHLD,
            Some(SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            }),
        )
        .unwrap();

    let echld = Arc::new(AtomicUsize::new(0));
    let reaped = Arc::new(AtomicUsize::new(0));
    let unexpected = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let finished_wq = Arc::new(WaitQueue::new());

    wait_test_sync::arm_register_barrier();

    let waiter = {
        let parent = parent.clone();
        let echld = echld.clone();
        let reaped = reaped.clone();
        let unexpected = unexpected.clone();
        let finished = finished.clone();
        let finished_wq = finished_wq.clone();
        ktask::spawn(move || {
            match wait_on_matching_children(
                &parent,
                WaitPid::Pid(child_pid),
                core::ptr::null_mut(),
                WaitOptions::empty(),
            )
            .map_err(LinuxError::from)
            {
                Err(LinuxError::ECHILD) => {
                    echld.fetch_add(1, Ordering::AcqRel);
                }
                Ok(pid) if pid == child_pid as isize => {
                    reaped.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    unexpected.fetch_add(1, Ordering::AcqRel);
                }
            }
            finished.fetch_add(1, Ordering::AcqRel);
            finished_wq.notify_one(true);
        })
    };

    while wait_test_sync::register_barrier_arrivals() < 1 {
        ktask::yield_now();
    }
    assert!(
        complete_sigchld_child_exit(&parent, &child),
        "explicit SIG_IGN must request autoreap"
    );
    wait_test_sync::release_register_barrier();

    finished_wq.wait_until(|| finished.load(Ordering::Acquire) == 1);
    wait_test_sync::disarm_register_barrier();
    assert_eq!(waiter.join(), 0);
    signals
        .set_signal_action(Signo::SIGCHLD, old_action)
        .unwrap();

    assert_eq!(echld.load(Ordering::Acquire), 1);
    assert_eq!(
        reaped.load(Ordering::Acquire),
        0,
        "autoreaped child must not be returned from wait"
    );
    assert_eq!(unexpected.load(Ordering::Acquire), 0);
    assert!(
        matching_children(&parent, WaitPid::Pid(child_pid), WaitOptions::empty()).is_empty(),
        "autoreap must remove the child before parent waiters resume"
    );
}

#[def_test(user, serial)]
fn test_sigchld_nocldwait_autoreap_leaves_no_waitable_child() {
    let parent = current_user_process();
    let child = parent.fork(8_231);
    let child_pid = child.pid();
    let signals = parent
        .signal_manager()
        .expect("current process signal manager");
    let old_action = signals
        .replace_signal_action(
            Signo::SIGCHLD,
            Some(SignalAction {
                flags: SignalActionFlags::NOCLDWAIT,
                ..Default::default()
            }),
        )
        .unwrap();

    assert!(
        complete_sigchld_child_exit(&parent, &child),
        "SA_NOCLDWAIT must request autoreap"
    );

    let err = wait_on_matching_children(
        &parent,
        WaitPid::Pid(child_pid),
        core::ptr::null_mut(),
        WaitOptions::WNOHANG,
    )
    .expect_err("autoreaped child must not remain waitable");
    signals
        .set_signal_action(Signo::SIGCHLD, old_action)
        .unwrap();

    assert_eq!(LinuxError::from(err), LinuxError::ECHILD);
}
