// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-owned advisory locks. Lock order is inode context, then blocked owners.
//! Installed locks own no file/table references; descriptor close and final file
//! release remove their owners before those identities can be reused.

use kerrno::{KError, KResult, LinuxError};
use ksync::Mutex;

use crate::VfsFile;

mod node;
use node::{LockList, OwnedLock, Waiter, WakeQueue};

/// Linux files-owner or open-file-owner identity, without retaining that owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileLockOwner {
    /// Traditional record locks owned by a shared descriptor table.
    Posix(usize),
    /// Record locks owned by an open file description.
    Ofd(usize),
    /// Whole-file BSD locks, in a separate conflict domain.
    Flock(usize),
}

impl FileLockOwner {
    /// Identifies a descriptor table at its stable address. The caller must use
    /// the same identity for close and keep the table alive through each operation.
    pub fn posix<T>(files: &T) -> Self {
        Self::Posix(core::ptr::from_ref(files).addr())
    }

    /// Identifies an open file description for OFD record locking.
    pub fn ofd(file: &VfsFile) -> Self {
        Self::Ofd(core::ptr::from_ref(file).addr())
    }

    /// Identifies an open file description for BSD whole-file locking.
    pub fn flock(file: &VfsFile) -> Self {
        Self::Flock(core::ptr::from_ref(file).addr())
    }

    fn is_posix(self) -> bool {
        matches!(self, Self::Posix(_))
    }

    fn is_flock(self) -> bool {
        matches!(self, Self::Flock(_))
    }
}

/// Advisory lock mode. Unlock requests are never installed in the inode list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLockType {
    /// Shared read lock.
    Read,
    /// Exclusive write lock.
    Write,
    /// Remove this owner's locks in the requested range.
    Unlock,
}

/// A normalized request or detached query result, without shared mutable state.
/// Installed records are private nodes owned by the inode's lock lists.
#[derive(Clone, Copy)]
pub struct FileLock {
    owner: FileLockOwner,
    kind: FileLockType,
    bounds: (i64, i64),
    pid: i32,
}

impl FileLock {
    /// Creates a request at an absolute file offset. Zero length extends through
    /// the maximum file offset; negative length describes bytes before `start`.
    /// Returns EINVAL for negative offsets and EOVERFLOW for an overflowing end.
    pub fn new(
        owner: FileLockOwner,
        pid: i32,
        kind: FileLockType,
        start: i64,
        length: i64,
    ) -> KResult<Self> {
        if start < 0 {
            return Err(KError::InvalidInput);
        }
        let (start, end) = match length.cmp(&0) {
            core::cmp::Ordering::Greater => (
                start,
                start
                    .checked_add(length - 1)
                    .ok_or(KError::from(LinuxError::EOVERFLOW))?,
            ),
            core::cmp::Ordering::Less => {
                let lower = start
                    .checked_add(length)
                    .filter(|offset| *offset >= 0)
                    .ok_or(KError::InvalidInput)?;
                (lower, start - 1)
            }
            core::cmp::Ordering::Equal => (start, i64::MAX),
        };
        Ok(Self::range(owner, pid, kind, start, end))
    }

    fn range(owner: FileLockOwner, pid: i32, kind: FileLockType, start: i64, end: i64) -> Self {
        Self {
            owner,
            kind,
            bounds: (start, end),
            pid,
        }
    }

    pub(crate) fn unlock(owner: FileLockOwner) -> Self {
        Self::range(owner, 0, FileLockType::Unlock, 0, i64::MAX)
    }

    /// Returns the lock's owner identity.
    pub fn owner(&self) -> FileLockOwner {
        self.owner
    }

    /// Returns the lock mode.
    pub fn kind(&self) -> FileLockType {
        self.kind
    }

    /// Returns the absolute first byte.
    pub fn start(&self) -> i64 {
        self.bounds().0
    }

    fn end(&self) -> i64 {
        self.bounds().1
    }

    fn bounds(&self) -> (i64, i64) {
        self.bounds
    }

    /// Returns the ABI length; zero denotes the maximum offset.
    pub fn length(&self) -> i64 {
        if self.end() == i64::MAX {
            0
        } else {
            self.end() - self.start() + 1
        }
    }

    /// Returns the reporting PID, or -1 for an OFD lock.
    pub fn pid(&self) -> i32 {
        if matches!(self.owner, FileLockOwner::Ofd(_)) {
            -1
        } else {
            self.pid
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.start() <= other.end() && other.start() <= self.end()
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        self.owner != other.owner
            && self.overlaps(other)
            && (self.kind == FileLockType::Write || other.kind == FileLockType::Write)
    }
}

/// Per-inode advisory lock context, corresponding to Linux `file_lock_context`.
/// All operations require sleepable task context. Never hold the descriptor
/// table lock across a blocking acquisition.
pub(crate) struct FileLockContext {
    state: Mutex<FileLockLists>,
}

// The guard groups Linux flc_posix/flc_flock under one flc_lock. Keeping the
// guarded value separate is required by Rust's guard-based mutex API.
struct FileLockLists {
    posix: LockList,
    flock: LockList,
}

impl FileLockContext {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(FileLockLists {
                posix: LockList::new(),
                flock: LockList::new(),
            }),
        }
    }

    pub(crate) fn test(&self, request: &FileLock) -> Option<FileLock> {
        let state = self.state.lock();
        state.posix.iter().find(|lock| {
            if request.kind == FileLockType::Unlock {
                request.owner == lock.owner && request.overlaps(lock)
            } else {
                request.conflicts_with(lock)
            }
        })
    }

    pub(crate) fn apply(&self, request: &FileLock, is_blocking: bool) -> KResult<()> {
        let mut waiter = None;
        let mut wake = WakeQueue::new();
        loop {
            let result = self.try_apply(request, is_blocking, &mut waiter, &mut wake);
            wake.wake();
            match result {
                Ok(true) => {
                    let result = waiter
                        .as_ref()
                        .expect("blocked request")
                        .wait_interruptible();
                    if result.is_ok() {
                        // Keep this request's children: a successful retry will
                        // transfer them to the installed lock without waking.
                        continue;
                    }
                    waiter
                        .as_ref()
                        .expect("blocked request")
                        .cancel_waiter(&mut wake);
                    wake.wake();
                    return result;
                }
                result => {
                    if let Some(waiter) = &waiter {
                        waiter.cancel_waiter(&mut wake);
                        wake.wake();
                    }
                    return result.map(|_| ());
                }
            }
        }
    }

    fn try_apply(
        &self,
        request: &FileLock,
        is_blocking: bool,
        waiter: &mut Option<Waiter>,
        wake: &mut WakeQueue,
    ) -> KResult<bool> {
        let mut state = self.state.lock();
        let list = if request.owner.is_flock() {
            &mut state.flock
        } else {
            &mut state.posix
        };
        // flock conversion drops the old lock before checking conflicts;
        // record-lock conversion preserves it on failure.
        if request.owner.is_flock() {
            if let Some(lock) =
                list.find(|lock| lock.owner == request.owner && lock.kind == request.kind)
            {
                if let Some(waiter) = waiter {
                    waiter.move_blocked(&lock);
                }
                return Ok(false);
            }
            list.remove_owner(request.owner, wake);
        }
        if request.kind == FileLockType::Unlock && request.start() == 0 && request.end() == i64::MAX
        {
            list.remove_owner(request.owner, wake);
            return Ok(false);
        }
        if request.kind != FileLockType::Unlock
            && let Some(lock) = list.find(|lock| request.conflicts_with(lock))
        {
            if !is_blocking {
                return Err(KError::WouldBlock);
            }
            let waiter = waiter.get_or_insert_with(|| Waiter::new(*request));
            waiter.block_on(lock, wake)?;
            return Ok(true);
        }
        if let Some(lock) = list.find(|lock| {
            lock.owner == request.owner
                && lock.kind == request.kind
                && lock.start() <= request.start()
                && lock.end() >= request.end()
        }) {
            if let Some(waiter) = waiter {
                waiter.move_blocked(&lock);
            }
        } else {
            state.update(request, waiter.as_ref(), wake);
        }
        Ok(false)
    }
}

impl FileLockLists {
    fn update(&mut self, request: &FileLock, waiter: Option<&Waiter>, wake: &mut WakeQueue) {
        let mut list = core::mem::take(if request.owner.is_flock() {
            &mut self.flock
        } else {
            &mut self.posix
        });
        let (mut start, mut end) = request.bounds();
        let mut remaining = LockList::new();
        let mut merged = None;
        while let Some(mut lock) = list.pop_front() {
            let record = lock.record();
            if record.owner != request.owner {
                remaining.push_back(lock);
                continue;
            }
            if record.kind == request.kind && record.end() >= start - 1 && record.start() - 1 <= end
            {
                start = start.min(record.start());
                end = end.max(record.end());
                if merged.is_none() {
                    // Preserve the first record's identity and descendants.
                    // Expansion needs neither allocation nor the global lock.
                    merged = Some(lock);
                } else {
                    lock.detach_blocked(wake);
                }
            } else if record.start() <= end && start <= record.end() {
                lock.detach_blocked(wake);
                if record.start() < start && record.end() > end {
                    let split = OwnedLock::new(FileLock::range(
                        record.owner,
                        record.pid,
                        record.kind,
                        end + 1,
                        record.end(),
                    ));
                    lock.set_bounds(record.start(), start - 1);
                    remaining.push_back(lock);
                    remaining.push_back(split);
                    Self::append_remaining(&mut list, &mut remaining);
                    break;
                }
                if record.start() < start {
                    lock.set_bounds(record.start(), start - 1);
                    remaining.push_back(lock);
                } else if record.end() > end {
                    lock.set_bounds(end + 1, record.end());
                    remaining.push_back(lock);
                }
            } else {
                remaining.push_back(lock);
            }
        }
        if request.kind != FileLockType::Unlock {
            let lock = if let Some(mut lock) = merged {
                lock.set_bounds(start, end);
                lock
            } else {
                OwnedLock::new(FileLock::range(
                    request.owner,
                    request.pid,
                    request.kind,
                    start,
                    end,
                ))
            };
            Self::install(&mut remaining, lock, waiter);
        }
        if request.owner.is_flock() {
            self.flock = remaining;
        } else {
            self.posix = remaining;
        }
    }

    fn append_remaining(source: &mut LockList, target: &mut LockList) {
        while let Some(lock) = source.pop_front() {
            target.push_back(lock);
        }
    }

    fn install(list: &mut LockList, lock: OwnedLock, waiter: Option<&Waiter>) {
        if let Some(waiter) = waiter {
            lock.accept_waiters(waiter);
        }
        let record = lock.record();
        let key = (record.owner, record.start());
        let mut sorted = LockList::new();
        while list
            .iter()
            .next()
            .is_some_and(|entry| (entry.owner, entry.start()) < key)
        {
            if let Some(entry) = list.pop_front() {
                sorted.push_back(entry);
            }
        }
        sorted.push_back(lock);
        Self::append_remaining(list, &mut sorted);
        *list = sorted;
    }
}

#[unittest::mod_test]
mod tests {
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
}
