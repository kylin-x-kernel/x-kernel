// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use unittest::def_test;

use super::*;

#[def_test]
fn record_lock_range_boundaries() {
    let owner = FileLockOwner::Posix(1);
    let backward = FileLock::new(owner, 1, FileLockType::Write, 40, -20).unwrap();
    assert_eq!((backward.start(), backward.length()), (20, 20));
    assert!(FileLock::new(owner, 1, FileLockType::Write, 0, -1).is_err());
    assert!(FileLock::new(owner, 1, FileLockType::Write, i64::MAX, 2).is_err());
    let eof = FileLock::new(owner, 1, FileLockType::Write, i64::MAX, 0).unwrap();
    assert_eq!(eof.length(), 0);
}

#[def_test]
fn record_lock_split_merge_and_failed_upgrade() {
    let context = FileLockContext::new();
    let owner = FileLockOwner::Posix(1);
    let second = FileLockOwner::Posix(2);
    context
        .apply(
            &FileLock::new(owner, 1, FileLockType::Write, 0, 100).unwrap(),
            false,
        )
        .unwrap();
    context
        .apply(
            &FileLock::new(owner, 1, FileLockType::Unlock, 20, 20).unwrap(),
            false,
        )
        .unwrap();
    let ranges = context
        .state
        .lock()
        .posix
        .iter()
        .map(|lock| (lock.start(), lock.end()))
        .collect::<Vec<_>>();
    assert_eq!(ranges, [(0, 19), (40, 99)]);
    context
        .apply(
            &FileLock::new(owner, 1, FileLockType::Write, 20, 20).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(context.state.lock().posix.iter().count(), 1);
    context
        .apply(
            &FileLock::new(owner, 1, FileLockType::Read, 0, 100).unwrap(),
            false,
        )
        .unwrap();
    context
        .apply(
            &FileLock::new(second, 2, FileLockType::Read, 0, 100).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(
        context.apply(
            &FileLock::new(owner, 1, FileLockType::Write, 0, 100).unwrap(),
            false
        ),
        Err(KError::WouldBlock)
    );
    assert!(
        context
            .state
            .lock()
            .posix
            .iter()
            .all(|lock| lock.kind == FileLockType::Read)
    );
}

#[def_test]
fn record_lock_matches_bytewise_updates() {
    let context = FileLockContext::new();
    let owner = FileLockOwner::Posix(3);
    let mut expected = [FileLockType::Unlock; 32];
    let mut seed = 17_u32;
    for _ in 0..128 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let start = (seed % 32) as usize;
        let end = start + ((seed >> 8) as usize % (32 - start));
        let kind = match seed % 3 {
            0 => FileLockType::Read,
            1 => FileLockType::Write,
            _ => FileLockType::Unlock,
        };
        context
            .apply(
                &FileLock::new(owner, 3, kind, start as i64, (end - start + 1) as i64).unwrap(),
                false,
            )
            .unwrap();
        expected[start..=end].fill(kind);
        let state = context.state.lock();
        for (offset, kind) in expected.iter().enumerate() {
            let actual = state
                .posix
                .iter()
                .find(|lock| lock.start() <= offset as i64 && offset as i64 <= lock.end())
                .map_or(FileLockType::Unlock, |lock| lock.kind);
            assert_eq!(actual, *kind);
        }
        let locks = state.posix.iter().collect::<Vec<_>>();
        for pair in locks.windows(2) {
            assert!(pair[0].end() < pair[1].start());
            assert!(pair[0].end() + 1 < pair[1].start() || pair[0].kind != pair[1].kind);
        }
    }
}

// Registration through the synchronous entry keeps every wait-tree
// observation in-task; the spawned child below only exercises the
// end-to-end signal path, whose resolution is deterministic because
// poll_interrupt double-checks the flag (a pre-scheduled interrupt is
// consumed at the child's first poll). No yield budget is involved, so a
// nested-VM scheduling stall can no longer produce a false failure
// (CI #2790/#2798).
#[def_test]
fn blocking_lock_wakeup_deadlock_and_signal_cleanup() {
    let first = Arc::new(FileLockContext::new());
    let second = Arc::new(FileLockContext::new());
    let owner_a = FileLockOwner::Posix(101);
    let owner_b = FileLockOwner::Posix(102);
    first
        .apply(
            &FileLock::new(owner_a, 101, FileLockType::Write, 0, 0).unwrap(),
            false,
        )
        .unwrap();
    second
        .apply(
            &FileLock::new(owner_b, 102, FileLockType::Write, 0, 0).unwrap(),
            false,
        )
        .unwrap();
    // A waiter registered on owner_b's lock makes the blocked owner visible
    // without any cross-task observation.
    let mut waiter = None;
    let mut wake = WakeQueue::new();
    assert_eq!(
        second.try_apply(
            &FileLock::new(owner_a, 101, FileLockType::Write, 0, 0).unwrap(),
            true,
            &mut waiter,
            &mut wake,
        ),
        Ok(true)
    );
    assert!(node::has_blocked_owner(owner_a));
    // Attach a waker so the unlock notification becomes observable, exactly
    // like a parked waiter would.
    assert!(waiter.as_ref().unwrap().poll_wait(Waker::noop()).is_pending());
    // The reverse request closes the cycle and must deadlock instead of
    // parking the caller.
    assert_eq!(
        first.apply(
            &FileLock::new(owner_b, 102, FileLockType::Write, 0, 0).unwrap(),
            true
        ),
        Err(LinuxError::EDEADLK.into())
    );
    // Unlocking hands the registration to the wake queue; the retry reuses
    // the same request node and now succeeds. try_apply is required here:
    // the apply() wrapper owns its internal wake queue, so the notification
    // would never reach ours.
    let mut unlock_waiter = None;
    assert_eq!(
        second.try_apply(
            &FileLock::unlock(owner_b),
            false,
            &mut unlock_waiter,
            &mut wake,
        ),
        Ok(false)
    );
    assert_eq!(wake.len(), 1);
    wake.wake();
    assert!(waiter.as_ref().unwrap().poll_wait(Waker::noop()).is_ready());
    assert_eq!(
        second.try_apply(
            &FileLock::new(owner_a, 101, FileLockType::Write, 0, 0).unwrap(),
            true,
            &mut waiter,
            &mut wake,
        ),
        Ok(false)
    );
    assert!(!node::has_blocked_owner(owner_a));
    // End-to-end signal path: the waiter parks on owner_a's installed lock
    // and the interrupt resolves it with ERESTARTSYS, cancelling its own
    // registration.
    let waiting = second.clone();
    let task = ktask::spawn(move || {
        assert_eq!(
            waiting.apply(
                &FileLock::new(owner_b, 102, FileLockType::Write, 0, 0).unwrap(),
                true
            ),
            Err(LinuxError::ERESTARTSYS.into())
        );
    });
    ktask::interrupt_task(&task, true);
    assert_eq!(task.join(), 0);
    assert!(!node::has_blocked_owner(owner_b));
    assert!(second.state.lock().posix.has_no_waiters());
}

#[def_test]
fn mixed_record_lock_upgrade_deadlock() {
    let context = FileLockContext::new();
    let posix = FileLockOwner::Posix(201);
    let ofd = FileLockOwner::Ofd(202);
    for owner in [posix, ofd] {
        context
            .apply(
                &FileLock::new(owner, 0, FileLockType::Read, 0, 0).unwrap(),
                false,
            )
            .unwrap();
    }
    let mut ofd_waiter = None;
    let mut posix_waiter = None;
    let mut wake = WakeQueue::new();
    let ofd_write = FileLock::new(ofd, 0, FileLockType::Write, 0, 0).unwrap();
    let posix_write = FileLock::new(posix, 0, FileLockType::Write, 0, 0).unwrap();
    assert_eq!(
        context.try_apply(&ofd_write, true, &mut ofd_waiter, &mut wake),
        Ok(true)
    );
    assert!(node::has_blocked_owner(ofd));
    assert_eq!(
        context.try_apply(&posix_write, true, &mut posix_waiter, &mut wake),
        Err(LinuxError::EDEADLK.into())
    );
    posix_waiter.as_ref().unwrap().cancel_waiter(&mut wake);
    ofd_waiter.as_ref().unwrap().cancel_waiter(&mut wake);
    assert!(!node::has_blocked_owner(ofd));

    // Reversing the upgrade order must still allow the OFD request to
    // block: it participates in the graph but does not detect deadlocks.
    assert_eq!(
        context.try_apply(&posix_write, true, &mut posix_waiter, &mut wake),
        Ok(true)
    );
    assert_eq!(
        context.try_apply(&ofd_write, true, &mut ofd_waiter, &mut wake),
        Ok(true)
    );
    ofd_waiter.unwrap().cancel_waiter(&mut wake);
    posix_waiter.unwrap().cancel_waiter(&mut wake);
    wake.wake();
}

#[def_test]
fn blocked_tree_transfer_requeue_and_cancel() {
    let context = FileLockContext::new();
    let holder = FileLockOwner::Posix(300);
    context
        .apply(
            &FileLock::new(holder, 0, FileLockType::Write, 0, 1).unwrap(),
            false,
        )
        .unwrap();
    let requests = (301..333)
        .map(|owner| {
            FileLock::new(FileLockOwner::Posix(owner), 0, FileLockType::Write, 0, 0).unwrap()
        })
        .collect::<Vec<_>>();
    let mut waiters = Vec::new();
    let mut wake = WakeQueue::new();
    for request in &requests {
        let mut waiter = None;
        assert_eq!(
            context.try_apply(request, true, &mut waiter, &mut wake),
            Ok(true)
        );
        assert!(
            waiter
                .as_ref()
                .unwrap()
                .poll_wait(Waker::noop())
                .is_pending()
        );
        waiters.push(waiter);
    }
    let mut expansion = None;
    assert_eq!(
        context.try_apply(
            &FileLock::new(holder, 0, FileLockType::Write, 0, 0).unwrap(),
            false,
            &mut expansion,
            &mut wake,
        ),
        Ok(false)
    );
    assert!(wake.is_empty());
    // A chain, not 32 direct children of the installed lock.
    {
        let state = context.state.lock();
        assert_eq!(state.posix.find(|_| true).unwrap().children_count(), 1);
        for pair in waiters.windows(2) {
            assert!(
                pair[1]
                    .as_ref()
                    .unwrap()
                    .is_blocked_by(pair[0].as_ref().unwrap())
            );
        }
    }
    // Deadlock search must reach the installed root through all 32
    // requests; the bounded owner search does not limit tree depth.
    let second = FileLockContext::new();
    second.apply(requests.last().unwrap(), false).unwrap();
    let mut attempt = None;
    assert_eq!(
        second.try_apply(
            &FileLock::new(holder, 0, FileLockType::Write, 0, 0).unwrap(),
            true,
            &mut attempt,
            &mut wake
        ),
        Err(LinuxError::EDEADLK.into())
    );
    attempt.unwrap().cancel_waiter(&mut wake);

    let mut unlock_waiter = None;
    context
        .try_apply(
            &FileLock::unlock(holder),
            false,
            &mut unlock_waiter,
            &mut wake,
        )
        .unwrap();
    assert_eq!(wake.len(), 1);
    wake.wake();
    for (index, request) in requests.iter().enumerate() {
        assert!(!waiters[index].as_ref().unwrap().is_blocked());
        assert_eq!(
            context.try_apply(request, true, &mut waiters[index], &mut wake),
            Ok(false)
        );
        waiters[index].as_ref().unwrap().cancel_waiter(&mut wake);
        // Granting transfers children without waking them.
        assert!(wake.is_empty());
        context
            .try_apply(
                &FileLock::unlock(request.owner),
                false,
                &mut unlock_waiter,
                &mut wake,
            )
            .unwrap();
        assert_eq!(wake.len(), usize::from(index + 1 < requests.len()));
        wake.wake();
    }

    context
        .apply(
            &FileLock::new(holder, 0, FileLockType::Write, 0, 0).unwrap(),
            false,
        )
        .unwrap();
    for (request, waiter) in requests.iter().zip(&mut waiters).take(3) {
        assert_eq!(
            context.try_apply(request, true, waiter, &mut wake),
            Ok(true)
        );
        assert!(
            waiter
                .as_ref()
                .unwrap()
                .poll_wait(Waker::noop())
                .is_pending()
        );
    }
    // Cancelling a middle node detaches it directly and wakes its child,
    // leaving the ancestor blocked. A failed retry reuses that child.
    let child = waiters[2].as_ref().unwrap().clone();
    waiters[1].as_ref().unwrap().cancel_waiter(&mut wake);
    assert_eq!(wake.len(), 1);
    assert!(waiters[0].as_ref().unwrap().is_blocked());
    // Pending notification still owns the child's links. Even a
    // spurious poll must wait for dequeue before it can retry.
    assert!(child.poll_wait(Waker::noop()).is_pending());
    wake.wake();
    assert!(!child.is_blocked());
    assert_eq!(
        context.try_apply(&requests[2], true, &mut waiters[2], &mut wake),
        Ok(true)
    );
    assert!(child.ptr_eq(waiters[2].as_ref().unwrap()));
    // A late poll after requeue must observe the new blocker.
    assert!(child.poll_wait(Waker::noop()).is_pending());
    wake.wake();
    assert!(child.is_blocked());
    for waiter in waiters.iter().take(3).flatten() {
        waiter.cancel_waiter(&mut wake);
    }
    wake.wake();
}

// The callback's try_lock checks require unrelated lock tests to be idle.
#[def_test(serial)]
fn expansion_preserves_identity_and_close_hands_off_readers() {
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct WakeCount {
        context: Arc<FileLockContext>,
        count: AtomicUsize,
    }
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            assert!(self.context.state.try_lock().is_some());
            assert!(node::wait_tree_unlocked());
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
    let context = Arc::new(FileLockContext::new());
    let count = Arc::new(WakeCount {
        context: context.clone(),
        count: AtomicUsize::new(0),
    });
    let waker = Waker::from(count.clone());
    let holder = FileLockOwner::Posix(400);
    context
        .apply(
            &FileLock::new(holder, 0, FileLockType::Write, 10, 10).unwrap(),
            false,
        )
        .unwrap();
    let original = context
        .state
        .lock()
        .posix
        .find(|_| true)
        .unwrap()
        .downgrade();
    let mut readers = Vec::new();
    let mut wake = WakeQueue::new();
    for id in 401..465 {
        let request =
            FileLock::new(FileLockOwner::Posix(id), 0, FileLockType::Read, 10, 10).unwrap();
        let mut waiter = None;
        assert_eq!(
            context.try_apply(&request, true, &mut waiter, &mut wake),
            Ok(true)
        );
        let waiter = waiter.unwrap();
        assert!(waiter.poll_wait(&waker).is_pending());
        readers.push(waiter);
    }
    context
        .apply(
            &FileLock::new(holder, 0, FileLockType::Write, 0, 40).unwrap(),
            false,
        )
        .unwrap();
    {
        let state = context.state.lock();
        let installed = state.posix.find(|_| true).unwrap();
        assert!(original.upgrade().unwrap().ptr_eq(&installed));
        assert_eq!(state.posix.iter().next().unwrap().bounds(), (0, 39));
        assert_eq!(installed.children_count(), 64);
        for reader in &readers {
            assert!(reader.is_blocked_by_node(&installed));
        }
    }
    let mut waiter = None;
    assert_eq!(
        context.try_apply(&FileLock::unlock(holder), false, &mut waiter, &mut wake),
        Ok(false)
    );
    assert_eq!(wake.len(), 64);
    assert!(context.state.lock().posix.is_empty());
    assert!(original.upgrade().is_none());
    assert!(readers[0].poll_wait(&waker).is_pending());
    // A signal may cancel a request whose notification is already in
    // flight. Its existing node must remain alive until the handoff ends.
    let cancelled = readers.pop().unwrap();
    cancelled.cancel_waiter(&mut WakeQueue::new());
    drop(cancelled);
    wake.wake();
    assert_eq!(count.count.load(Ordering::Relaxed), 64);
    assert!(
        readers
            .iter()
            .all(|reader| reader.poll_wait(&waker).is_ready())
    );
}

// The regression this test guards is the uncontended file-lock path taking
// the global wait-tree mutex (BLOCKED_OWNERS). Run the uncontended operation
// sequence on the task that holds that mutex: any acquisition attempt
// re-enters it and trips the mutex's self-ownership assert, so the property
// is checked deterministically. The previous spawned-child-plus-yield-budget
// shape only measured guest scheduling latency, which a nested VM may starve
// independently of kernel correctness (CI #2790/#2798).
#[def_test]
fn uncontended_operations_do_not_take_wait_tree_lock() {
    let blocked = node::hold_wait_tree();
    for owner in [FileLockOwner::Posix(500), FileLockOwner::Flock(500)] {
        let context = FileLockContext::new();
        for (kind, start, length) in [
            (FileLockType::Write, 0, 10),
            (FileLockType::Write, 0, 20),
            (FileLockType::Write, 30, 10),
            (FileLockType::Write, 0, 40),
            (FileLockType::Unlock, 10, 10),
            (FileLockType::Read, 0, 40),
            (FileLockType::Unlock, 0, 0),
        ] {
            context
                .apply(
                    &FileLock::new(owner, 0, kind, start, length).unwrap(),
                    false,
                )
                .unwrap();
        }
    }
    drop(blocked);
}

#[def_test]
fn flock_domain_and_conversion() {
    let context = FileLockContext::new();
    let first = FileLockOwner::Flock(1);
    let second = FileLockOwner::Flock(2);
    context
        .apply(
            &FileLock::new(FileLockOwner::Posix(1), 1, FileLockType::Write, 0, 0).unwrap(),
            false,
        )
        .unwrap();
    for owner in [first, second] {
        context
            .apply(
                &FileLock::new(owner, 1, FileLockType::Read, 0, 0).unwrap(),
                false,
            )
            .unwrap();
    }
    assert_eq!(
        context.apply(
            &FileLock::new(first, 1, FileLockType::Write, 0, 0).unwrap(),
            false
        ),
        Err(KError::WouldBlock)
    );
    let state = context.state.lock();
    assert_eq!(state.flock.iter().count(), 1);
    assert_eq!(state.flock.iter().next().unwrap().owner, second);
}

fn lease_test_inode() -> Arc<VfsInode> {
    struct NoopInodeOperations;
    impl crate::InodeOperations for NoopInodeOperations {}
    struct NoopFileOperations;
    impl crate::FileOperations for NoopFileOperations {}
    VfsInode::new_file_with_operations(
        Arc::new(()),
        Arc::new(NoopInodeOperations),
        Arc::new(NoopFileOperations),
        crate::NodeFlags::empty(),
        crate::VfsInodeInit::new(
            2,
            0,
            crate::Umode::new(
                crate::NodeType::RegularFile,
                crate::NodePermission::default(),
            ),
        ),
    )
}

fn lease_request(owner: FileLockOwner, kind: FileLockType) -> FileLock {
    // An impossible pid keeps the best-effort break SIGIO undelivered here.
    FileLock::new(owner, i32::MAX, kind, 0, 0).unwrap()
}

#[def_test]
fn lease_set_get_and_unlock_roundtrip() {
    let context = FileLockContext::new();
    let inode = lease_test_inode();
    let owner = FileLockOwner::Ofd(1);
    // F_UNLCK without a lease is EAGAIN (Linux generic_delete_lease).
    assert_eq!(
        context.set_lease(
            &inode,
            &lease_request(owner, FileLockType::Unlock),
            3,
            0,
            0
        ),
        Err(KError::WouldBlock)
    );
    assert_eq!(context.get_lease(owner), FileLockType::Unlock);
    for kind in [FileLockType::Read, FileLockType::Write] {
        context
            .set_lease(&inode, &lease_request(owner, kind), 3, 0, 0)
            .unwrap();
        assert_eq!(context.get_lease(owner), kind);
    }
    // Type change modifies in place; the record-lock domain stays separate.
    context
        .set_lease(
            &inode,
            &lease_request(owner, FileLockType::Read),
            3,
            0,
            0,
        )
        .unwrap();
    assert_eq!(context.get_lease(owner), FileLockType::Read);
    assert!(
        context
            .test(&lease_request(FileLockOwner::Posix(9), FileLockType::Write))
            .is_none()
    );
    context
        .set_lease(
            &inode,
            &lease_request(owner, FileLockType::Unlock),
            3,
            0,
            0,
        )
        .unwrap();
    assert_eq!(context.get_lease(owner), FileLockType::Unlock);
    assert!(context.state.lock().lease.lease_iter().next().is_none());
}

#[def_test]
fn lease_exclusivity_and_open_conflicts() {
    let context = FileLockContext::new();
    let inode = lease_test_inode();
    let writer = FileLockOwner::Ofd(1);
    let reader = FileLockOwner::Ofd(2);
    // Write-lease open conflict: only this file's own write count is allowed.
    inode.acquire_write_access();
    assert_eq!(
        context.set_lease(&inode, &lease_request(writer, FileLockType::Write), 3, 0, 0),
        Err(KError::WouldBlock)
    );
    context
        .set_lease(&inode, &lease_request(writer, FileLockType::Write), 3, 1, 0)
        .unwrap();
    // A read lease refuses any write open, including the requestor's own.
    assert_eq!(
        context.set_lease(&inode, &lease_request(reader, FileLockType::Read), 4, 0, 1),
        Err(KError::WouldBlock)
    );
    // The write lease excludes other write leases...
    assert_eq!(
        context.set_lease(&inode, &lease_request(reader, FileLockType::Write), 4, 1, 0),
        Err(KError::WouldBlock)
    );
    inode.release_write_access();
    // ...while a read open outside the requestor still blocks the exclusive
    // count check for write leases.
    inode.acquire_read_access();
    assert_eq!(
        context.set_lease(&inode, &lease_request(writer, FileLockType::Write), 3, 0, 0),
        Err(KError::WouldBlock)
    );
    // Linux generic_add_lease tolerates a second read lease beside a write
    // lease that is not pending removal.
    context
        .set_lease(&inode, &lease_request(reader, FileLockType::Read), 4, 0, 1)
        .unwrap();
    assert_eq!(context.get_lease(reader), FileLockType::Read);
    assert_eq!(context.get_lease(writer), FileLockType::Write);
}

#[def_test]
fn lease_break_marks_pending_and_times_out() {
    let context = FileLockContext::new();
    let inode = lease_test_inode();
    let owner = FileLockOwner::Ofd(1);
    context
        .set_lease(&inode, &lease_request(owner, FileLockType::Write), 3, 0, 0)
        .unwrap();
    // A nonblocking read break marks the downgrade and reports EWOULDBLOCK;
    // get_lease answers with the post-break target.
    assert_eq!(
        context.break_leases(false, true),
        Err(KError::WouldBlock)
    );
    assert_eq!(context.get_lease(owner), FileLockType::Read);
    // A pending downgrade still blocks other write leases.
    let other = FileLockOwner::Ofd(2);
    assert_eq!(
        context.set_lease(&inode, &lease_request(other, FileLockType::Write), 4, 0, 0),
        Err(KError::WouldBlock)
    );
    // The write breaker promotes the mark to unlock-pending...
    assert_eq!(
        context.break_leases(true, true),
        Err(KError::WouldBlock)
    );
    let state = context.state.lock();
    let (_, lease) = state.lease.lease_iter().next().unwrap();
    assert!(matches!(
        lease.pending,
        Some(LeasePending::Unlock { .. })
    ));
    drop(state);
    assert_eq!(context.get_lease(owner), FileLockType::Unlock);
    // ...and once the grace period runs out the lease is gone entirely.
    let mut state = context.state.lock();
    let mut lease = state
        .lease
        .take_matching(|lock| lock.owner == owner)
        .unwrap();
    lease.set_lease_state(LeaseState {
        fd: 3,
        pending: Some(LeasePending::Unlock {
            break_time: ktime_types::MonotonicInstant::ORIGIN,
        }),
    });
    FileLockLists::install(&mut state.lease, lease, None);
    drop(state);
    assert_eq!(context.get_lease(owner), FileLockType::Unlock);
    assert!(context.state.lock().lease.lease_iter().next().is_none());
    // An expired downgrade instead shrinks the lease to a read lease.
    context
        .set_lease(&inode, &lease_request(owner, FileLockType::Write), 3, 0, 0)
        .unwrap();
    let mut state = context.state.lock();
    let mut lease = state
        .lease
        .take_matching(|lock| lock.owner == owner)
        .unwrap();
    lease.set_lease_state(LeaseState {
        fd: 3,
        pending: Some(LeasePending::Downgrade {
            downgrade_time: ktime_types::MonotonicInstant::ORIGIN,
        }),
    });
    FileLockLists::install(&mut state.lease, lease, None);
    drop(state);
    assert_eq!(context.get_lease(owner), FileLockType::Read);
    let state = context.state.lock();
    let (lock, lease) = state.lease.lease_iter().next().unwrap();
    assert_eq!(lock.kind, FileLockType::Read);
    assert!(lease.pending.is_none());
}

#[def_test]
fn lease_open_close_accounting_flows_through_production_paths() {
    // The write-lease exclusivity check consumes the inode open counts; this
    // drives the production producers (do_dentry_open, alloc_clone,
    // alloc_clone_with_private_data, Mount::alloc_file_pseudo) and the
    // VfsFile::Drop releases, not hand-installed counts.
    use crate::{Dentry, FMode, OpenFlags, Path, VfsFileBuilder};
    struct NoopFileOperations;
    impl crate::FileOperations for NoopFileOperations {}
    let context = FileLockContext::new();
    let inode = lease_test_inode();
    let super_block = crate::nullfs::new_superblock();
    let mount = crate::Mount::new_root(&super_block);
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, "leased".into());
    dentry.bind_super_block(&super_block);
    let path = Path::new(mount.clone(), dentry);
    let ops: Arc<dyn crate::FileOperations> = Arc::new(NoopFileOperations);

    let reader = VfsFileBuilder::allocate(OpenFlags::empty(), kcred::initial_cred())
        .unwrap()
        .vfs_open(path.clone())
        .unwrap();
    // do_dentry_open published i_readcount before the lease break, and the
    // exclusive write-lease check sees the live reader through it.
    assert_eq!(inode.read_count(), 1);
    let owner = FileLockOwner::Ofd(1);
    assert_eq!(
        context.set_lease(&inode, &lease_request(owner, FileLockType::Write), 3, 0, 0),
        Err(KError::WouldBlock)
    );
    // file_init_path-style producers: clone, clone with private data, pseudo.
    let clone = reader.alloc_clone(reader.mode(), reader.flags(), ops.clone()).unwrap();
    assert_eq!(inode.read_count(), 2);
    let clone_with_data = reader
        .alloc_clone_with_private_data(reader.mode(), reader.flags(), ops.clone(), alloc::sync::Arc::new(0u8))
        .unwrap();
    assert_eq!(inode.read_count(), 3);
    let pseudo = mount
        .alloc_file_pseudo(inode.clone(), "pseudo", FMode::READ, OpenFlags::empty(), ops.clone(), kcred::initial_cred())
        .unwrap();
    assert_eq!(inode.read_count(), 4);
    // VfsFile::Drop releases each description's contribution; until then the
    // lease request keeps seeing the readers.
    assert_eq!(
        context.set_lease(&inode, &lease_request(owner, FileLockType::Write), 3, 0, 0),
        Err(KError::WouldBlock)
    );
    drop(clone);
    drop(clone_with_data);
    drop(pseudo);
    assert_eq!(inode.read_count(), 1);
    drop(reader);
    assert_eq!(inode.read_count(), 0);
    // With every production increment paired by a release, the same request
    // now succeeds (the consuming check, not just the counter, flipped).
    context
        .set_lease(&inode, &lease_request(owner, FileLockType::Write), 3, 0, 0)
        .unwrap();
    assert_eq!(context.get_lease(owner), FileLockType::Write);
}
