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
    let waiting = second.clone();
    let task = ktask::spawn(move || {
        waiting
            .apply(
                &FileLock::new(owner_a, 101, FileLockType::Write, 0, 0).unwrap(),
                true,
            )
            .unwrap();
    });
    for _ in 0..10000 {
        if node::has_blocked_owner(owner_a) {
            break;
        }
        ktask::yield_now();
    }
    assert!(node::has_blocked_owner(owner_a));
    assert_eq!(
        first.apply(
            &FileLock::new(owner_b, 102, FileLockType::Write, 0, 0).unwrap(),
            true
        ),
        Err(LinuxError::EDEADLK.into())
    );
    second.apply(&FileLock::unlock(owner_b), false).unwrap();
    assert_eq!(task.join(), 0);
    assert!(!node::has_blocked_owner(owner_a));
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
    for _ in 0..10000 {
        if node::has_blocked_owner(owner_b) {
            break;
        }
        ktask::yield_now();
    }
    assert!(node::has_blocked_owner(owner_b));
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

#[def_test]
fn uncontended_operations_do_not_take_wait_tree_lock() {
    use core::sync::atomic::{AtomicBool, Ordering};

    let finished = Arc::new(AtomicBool::new(false));
    let completed = finished.clone();
    let blocked = node::hold_wait_tree();
    let task = ktask::spawn(move || {
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
        completed.store(true, Ordering::Release);
    });
    // Bound a regression's wait, then release the mutex before joining so
    // a failure is reported instead of hanging the test kernel.
    for _ in 0..10000 {
        if finished.load(Ordering::Acquire) {
            break;
        }
        ktask::yield_now();
    }
    let completed_without_global_lock = finished.load(Ordering::Acquire);
    drop(blocked);
    assert_eq!(task.join(), 0);
    assert!(completed_without_global_lock);
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
