// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-owned advisory locks. Lock order is inode context, then blocked owners.
//! Installed locks own no file/table references; descriptor close and final file
//! release remove their owners before those identities can be reused.

use alloc::vec::Vec;
use core::future::poll_fn;

use kerrno::{KError, KResult, LinuxError};
use ksignal::SignalInfo;
use ksync::Mutex;
use ktask::future::{block_on_interruptible, timeout_at};
use ktime_types::TimeSpan;

use crate::{VfsFile, VfsInode};

mod node;
use node::{LeasePending, LeaseState, LockList, OwnedLock, Waiter, WakeQueue};

/// Linux `lease_break_time`: seconds a pending break runs before the lease is
/// downgraded or removed by `time_out_leases`.
const LEASE_BREAK_TIME: u64 = 45;

/// Linux uapi `POLL_MSG`: the `si_band` value of lease-break `SIGIO`.
const POLL_MSG: i32 = 3;

/// Linux files-owner or open-file-owner identity, without retaining that owner.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

// The guard groups Linux flc_posix/flc_flock/flc_lease under one flc_lock.
// Keeping the guarded value separate is required by Rust's guard-based mutex
// API.
struct FileLockLists {
    posix: LockList,
    flock: LockList,
    lease: LockList,
}

impl FileLockContext {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(FileLockLists {
                posix: LockList::new(),
                flock: LockList::new(),
                lease: LockList::new(),
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

    /// Applies a file lease request; Linux `generic_setlease` + `generic_add_lease`.
    ///
    /// The request carries the whole-file range and the open file description's
    /// owner identity; `fd` becomes the descriptor reported in the break SIGIO.
    /// `self_wcount`/`self_rcount` are the requesting file's own contributions
    /// to the inode open counts (Linux `lease_open_conflict`). Counts are read
    /// inside the state mutex, so "check counts, then insert" is atomic against
    /// opens, whose own count increment always precedes their lease break under
    /// this lock. That replaces Linux's post-insert `smp_mb` re-check.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` (`EAGAIN`) for open-count or existing-lease
    /// conflicts, and forwards `F_UNLCK` to [`Self::delete_lease`].
    pub(crate) fn set_lease(
        &self,
        inode: &VfsInode,
        request: &FileLock,
        fd: i32,
        self_wcount: usize,
        self_rcount: usize,
    ) -> KResult<()> {
        if request.kind == FileLockType::Unlock {
            return self.delete_lease(request.owner);
        }
        let mut wake = WakeQueue::new();
        let result = {
            let mut state = self.state.lock();
            state.time_out_leases(&mut wake);
            let open_conflict = match request.kind {
                // An F_RDLCK lease refuses any write open, including the
                // requestor's own (Linux inode_is_open_for_write).
                FileLockType::Read => inode.write_count() != 0,
                FileLockType::Write => {
                    inode.write_count() != self_wcount || inode.read_count() != self_rcount
                }
                FileLockType::Unlock => false,
            };
            if open_conflict {
                Err(KError::WouldBlock)
            } else {
                state.add_lease(request, fd, &mut wake)
            }
        };
        wake.wake();
        result
    }

    /// Removes this owner's lease; Linux `generic_delete_lease`.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` (`EAGAIN`) when the owner holds no lease.
    pub(crate) fn delete_lease(&self, owner: FileLockOwner) -> KResult<()> {
        let mut wake = WakeQueue::new();
        let result = {
            let mut state = self.state.lock();
            match state.lease.take_matching(|lock| lock.owner == owner) {
                None => Err(KError::WouldBlock),
                Some(mut lease) => {
                    // lease_modify(F_UNLCK): wake blocked breakers, dispose node.
                    lease.detach_blocked(&mut wake);
                    Ok(())
                }
            }
        };
        wake.wake();
        result
    }

    /// Reports this owner's lease type; Linux `__fcntl_getlease`.
    pub(crate) fn get_lease(&self, owner: FileLockOwner) -> FileLockType {
        let mut wake = WakeQueue::new();
        let kind = {
            let mut state = self.state.lock();
            state.time_out_leases(&mut wake);
            state
                .lease
                .lease_iter()
                .find(|(lock, _)| lock.owner == owner)
                .map(|(lock, lease)| lease.target_type(lock.kind))
                .unwrap_or(FileLockType::Unlock)
        };
        wake.wake();
        kind
    }

    /// Revokes all leases conflicting with an open or truncate; Linux
    /// `__break_lease`.
    ///
    /// `want_write` breaks conflicting leases for removal (`FL_UNLOCK_PENDING`),
    /// otherwise for downgrade (`FL_DOWNGRADE_PENDING`); each newly pending
    /// lease receives the owner's break SIGIO. `nonblock` reports `WouldBlock`
    /// (`EWOULDBLOCK`) instead of waiting (Linux `LEASE_BREAK_NONBLOCK`).
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` for a nonblocking break that found a conflict, and
    /// `LinuxError::ERESTARTSYS` when an interruptible wait is signal-delivered.
    pub(crate) fn break_leases(&self, want_write: bool, nonblock: bool) -> KResult<()> {
        // The breaker owns no file (Linux flc_owner=NULL); a stack-local
        // identity stays unique and alive across the wait. Deadlock detection
        // and the global waiter index apply only to POSIX owners.
        let marker = ();
        let breaker = FileLock::range(
            FileLockOwner::Ofd(core::ptr::from_ref(&marker).addr()),
            0,
            if want_write {
                FileLockType::Write
            } else {
                FileLockType::Read
            },
            0,
            i64::MAX,
        );
        let mut waiter: Option<Waiter> = None;
        let mut wake = WakeQueue::new();
        let mut result = Ok(());
        loop {
            let deadline = {
                let mut state = self.state.lock();
                state.time_out_leases(&mut wake);
                state.mark_conflicting_leases(breaker.kind);
                let Some((blocker, lease)) = state.lease.first_blocking_lease(breaker.kind) else {
                    break;
                };
                if nonblock {
                    result = Err(KError::WouldBlock);
                    break;
                }
                // Register under the state mutex, the same discipline as
                // `try_apply`: a concurrent `delete_lease` detaches blocked
                // waiters under this mutex, so it can never drop a blocker
                // that is not yet visible (Linux inserts the block while
                // holding flc_lock, fs/locks.c __break_lease).
                let waiter = waiter.get_or_insert_with(|| Waiter::new(breaker));
                if let Err(error) = waiter.block_on(blocker, &mut wake) {
                    result = Err(error);
                    break;
                }
                lease.pending.map(LeasePending::deadline)
            };
            let waiter = waiter.as_mut().expect("break registered this iteration");
            wake.wake();
            // wait_event_interruptible_timeout against the pending deadline.
            // Linux re-times-out and restarts on expiry, rescan-reblocks on a
            // wake, and returns ERESTARTSYS on signal delivery.
            enum Waited {
                Ready,
                TimedOut,
                Interrupted(KError),
            }
            let outcome = match deadline {
                Some(deadline) => {
                    match block_on_interruptible(timeout_at(
                        Some(deadline),
                        poll_fn(|cx| waiter.poll_wait(cx.waker())),
                    )) {
                        Ok(Ok(())) => Waited::Ready,
                        Ok(Err(_elapsed)) => Waited::TimedOut,
                        Err(error) => Waited::Interrupted(if error.is_task_interrupt() {
                            LinuxError::ERESTARTSYS.into()
                        } else {
                            error.into()
                        }),
                    }
                }
                None => match waiter.wait_interruptible() {
                    Ok(()) => Waited::Ready,
                    Err(error) => Waited::Interrupted(error),
                },
            };
            // __locks_delete_block() unconditionally after every wait attempt.
            waiter.cancel_waiter(&mut wake);
            wake.wake();
            match outcome {
                Waited::Ready | Waited::TimedOut => continue,
                Waited::Interrupted(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        if let Some(waiter) = &waiter {
            waiter.cancel_waiter(&mut wake);
        }
        wake.wake();
        result
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

    /// Adds or modifies this owner's lease; Linux `generic_add_lease` minus the
    /// open-conflict check, which the caller performs in the same critical
    /// section. `F_UNLCK` requests never reach here.
    fn add_lease(&mut self, request: &FileLock, fd: i32, wake: &mut WakeQueue) -> KResult<()> {
        let mut mine = None;
        for (lock, lease) in self.lease.lease_iter() {
            if lock.owner == request.owner {
                mine = Some(lock);
                continue;
            }
            // A write lease refuses any other lease; a read lease also refuses
            // one already pending removal, but tolerates a pending downgrade.
            if request.kind == FileLockType::Write
                || matches!(lease.pending, Some(LeasePending::Unlock { .. }))
            {
                return Err(KError::WouldBlock);
            }
        }
        let Some(mine) = mine else {
            Self::install(
                &mut self.lease,
                OwnedLock::new_lease(*request, LeaseState { fd, pending: None }),
                None,
            );
            return Ok(());
        };
        // lease_modify() on the existing lease, plus lm_setup refreshing the
        // fasync descriptor to the caller's current fd.
        let mut lease = self
            .lease
            .take_matching(|lock| lock.owner == request.owner)
            .expect("the just-scanned lease is still held");
        let mut state = lease.lease_state();
        state.fd = fd;
        if request.kind == FileLockType::Read
            && matches!(state.pending, Some(LeasePending::Downgrade { .. }))
        {
            // lease_clear_pending(): the downgrade raced and is withdrawn.
            state.pending = None;
        }
        if lease.record().kind == request.kind {
            // Same type: keep the node identity and any blocked breakers.
            lease.set_lease_state(state);
            Self::install(&mut self.lease, lease, None);
            return Ok(());
        }
        // The kind is immutable per node; the replacement wakes blocked
        // breakers to re-check their conflict (lease_modify +
        // locks_wake_up_blocks).
        let record = FileLock::range(request.owner, mine.pid, request.kind, 0, i64::MAX);
        lease.detach_blocked(wake);
        Self::install(&mut self.lease, OwnedLock::new_lease(record, state), None);
        Ok(())
    }

    /// Retires leases whose break grace period expired; Linux
    /// `time_out_leases`. Called under the state mutex before lease lookups
    /// and lease breaks.
    fn time_out_leases(&mut self, wake: &mut WakeQueue) {
        if self.lease.iter().next().is_none() {
            return;
        }
        let now = ktime::realtime_and_monotonic().1;
        let expired: Vec<_> = self
            .lease
            .lease_iter()
            .filter_map(|(lock, lease)| {
                lease
                    .pending
                    .filter(|pending| pending.expired(now))
                    .map(|pending| (lock.owner, lock.pid, pending))
            })
            .collect();
        for (owner, pid, pending) in expired {
            let Some(mut lease) = self.lease.take_matching(|lock| lock.owner == owner) else {
                continue;
            };
            let mut state = lease.lease_state();
            state.pending = None;
            match pending {
                // The downgrade grace ran out: lease_modify(F_RDLCK).
                LeasePending::Downgrade { .. } => {
                    let record = FileLock::range(owner, pid, FileLockType::Read, 0, i64::MAX);
                    lease.detach_blocked(wake);
                    Self::install(&mut self.lease, OwnedLock::new_lease(record, state), None);
                }
                // The break grace ran out: lease_modify(F_UNLCK) removes it.
                LeasePending::Unlock { .. } => {
                    lease.detach_blocked(wake);
                }
            }
        }
    }

    /// Marks every lease conflicting with `breaker_kind` pending and notifies
    /// its owner; Linux `__break_lease`'s mark loop plus `lm_break`.
    fn mark_conflicting_leases(&mut self, breaker_kind: FileLockType) {
        if self.lease.iter().next().is_none() {
            return;
        }
        // leases_conflict() is type-only: a write side breaks anything.
        let conflicts =
            |kind: FileLockType| kind == FileLockType::Write || breaker_kind == FileLockType::Write;
        let now = ktime::realtime_and_monotonic().1;
        let deadline = now
            .checked_add(TimeSpan::from_secs(LEASE_BREAK_TIME))
            .unwrap_or(now);
        let targets: Vec<_> = self
            .lease
            .lease_iter()
            .filter_map(|(lock, state)| {
                if !conflicts(lock.kind) {
                    return None;
                }
                let pending = if breaker_kind == FileLockType::Write {
                    // Upgrade a pending downgrade; never move a lease already
                    // pending removal back in time.
                    (!matches!(state.pending, Some(LeasePending::Unlock { .. }))).then_some(
                        LeasePending::Unlock {
                            break_time: deadline,
                        },
                    )
                } else {
                    // Any pending mark already notifies the owner.
                    state.pending.is_none().then_some(LeasePending::Downgrade {
                        downgrade_time: deadline,
                    })
                };
                pending.map(|pending| (lock.owner, lock.pid, state.fd, pending))
            })
            .collect();
        for (owner, pid, fd, pending) in targets {
            let Some(mut lease) = self.lease.take_matching(|lock| lock.owner == owner) else {
                continue;
            };
            // Only the payload changes: the node keeps its identity and its
            // blocked breakers (Linux sets fl_flags in place, waking nobody).
            lease.set_lease_state(LeaseState {
                fd,
                pending: Some(pending),
            });
            Self::install(&mut self.lease, lease, None);
            // lm_break (lease_break_callback → kill_fasync SIGIO). Linux sends
            // it under flc_lock; a vanished or unreceptive owner is ignored,
            // as kill_fasync ignores delivery failures.
            let _ = ksignal::send_to_process(pid as u32, SignalInfo::new_sigio(POLL_MSG, fd));
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/locks.rs");
