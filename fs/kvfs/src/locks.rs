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

unittest::path_mod_test!(tests, "../ut-tests/locks.rs");
