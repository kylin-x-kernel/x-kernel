// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A reader-writer lock implementation.

use core::sync::atomic::{AtomicU32, Ordering};

use event_listener::{Event, listener};
use ktask::{LockOwner, LockOwnerState, future::block_on};

use crate::util::{Spin, SpinConfig};

const WRITE_LOCKED: u32 = 1 << 31;
const MAX_READERS: u32 = WRITE_LOCKED - 1;

/// Spin budget while no writer is recorded: readers hold the lock, or a writer
/// has not recorded itself yet.
///
/// [`RawMutex`](crate::RawMutex)'s default: three exponential pauses, then
/// `yield_now`. One budget covers a whole acquire. This stands in for Linux's
/// time-bounded reader-owned spin; `ktask`'s design notes list the differences.
const UNOWNED_SPIN: SpinConfig = SpinConfig {
    max_spins: 10,
    spin_before_yield: 3,
};

/// Pause-only budget for one spin attempt while the recorded writer runs.
///
/// Rebuilt on every attempt. It ends the spin of a task alone on its CPU,
/// where the tick never sets `needs_resched`.
const RUNNING_SPIN: SpinConfig = SpinConfig {
    max_spins: 10,
    spin_before_yield: 10,
};

/// A [`lock_api::RawRwLock`] implementation.
///
/// Allows multiple readers or a single writer.
/// The high bit of the state represents the write lock,
/// and the low 31 bits represent the reader count.
///
/// A contended acquire spins before sleeping while the recorded writer runs on
/// another CPU or readers hold the lock, and stops once a reschedule is
/// pending. Waiting does not reserve the lock: as with
/// [`RawMutex`](crate::RawMutex), any caller can take a released lock before a
/// woken waiter runs, so the lock does not promise starvation freedom.
///
/// # Panics
///
/// `lock_exclusive` and `lock_shared` panic when this task is the recorded
/// writer, as [`RawMutex`](crate::RawMutex) does on re-entry. Readers are not
/// recorded, so acquiring again while holding only read guards waits. A record
/// that stores the `TaskInner` address can panic a later task that reused the
/// address while the write record is still held.
pub struct RawRwLock {
    state: AtomicU32, // High bit: write lock, low 31 bits: reader count
    /// Writer holding the lock, for spinners. Readers do not record themselves.
    owner: LockOwner,
    writer_event: Event,
    reader_event: Event,
    #[cfg(feature = "stats")]
    stats: &'static klockstat::LockClassStats,
}

impl RawRwLock {
    /// Creates a new [`RawRwLock`].
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            owner: LockOwner::new(),
            writer_event: Event::new(),
            reader_event: Event::new(),
            #[cfg(feature = "stats")]
            stats: &klockstat::NOOP_CLASS,
        }
    }
}

impl Default for RawRwLock {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `RawRwLock` enforces the shared/exclusive access protocol required
// by `lock_api::RawRwLock` through its internal state machine.
unsafe impl lock_api::RawRwLock for RawRwLock {
    type GuardMarker = lock_api::GuardSend;

    const INIT: Self = RawRwLock::new();

    #[inline]
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Overflow needs 2^31 - 1 simultaneously \
                  held read guards, far beyond any number of tasks; std's RwLock likewise panics \
                  on reader overflow"
    )]
    fn lock_shared(&self) {
        #[cfg(feature = "stats")]
        let mut did_block = false;
        let mut unowned = Spin::new(UNOWNED_SPIN);

        loop {
            let state = self.state.load(Ordering::Relaxed);

            // Check if write locked
            if state & WRITE_LOCKED != 0 {
                if self.spin_to_acquire(&mut unowned, || self.try_acquire_read()) {
                    #[cfg(feature = "stats")]
                    self.record_read_acquisitions();
                    return;
                }
                listener!(self.reader_event => listener);
                if self.state.load(Ordering::Acquire) & WRITE_LOCKED != 0 {
                    #[cfg(feature = "stats")]
                    if !did_block {
                        self.record_read_contentions();
                        did_block = true;
                    }
                    block_on(listener);
                }
                continue;
            }

            // Check reader count
            if state >= MAX_READERS {
                panic!("too many readers");
            }

            // Try to increment reader count
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    #[cfg(feature = "stats")]
                    self.record_read_acquisitions();
                    return;
                }
                Err(_) => continue,
            }
        }
    }

    #[inline]
    fn try_lock_shared(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);

        if state & WRITE_LOCKED != 0 || state >= MAX_READERS {
            return false;
        }

        // Using strong compare_exchange here since this is a single-shot attempt
        // without retry loop, unlike lock_shared which uses _weak in a loop
        let acquired = self
            .state
            .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            #[cfg(feature = "stats")]
            self.record_read_acquisitions();
        }
        acquired
    }

    #[inline]
    unsafe fn unlock_shared(&self) {
        // SAFETY: `lock_api` only calls `unlock_shared` for a previously
        // acquired shared lock, so decrementing the reader count and waking a
        // writer when the last reader leaves preserves the raw rwlock protocol.
        let state = self.state.fetch_sub(1, Ordering::Release);

        // Wake up a waiting writer if this was the last reader
        if state == 1 {
            self.writer_event.notify(1);
        }
    }

    #[inline]
    fn lock_exclusive(&self) {
        #[cfg(feature = "stats")]
        let mut did_block = false;
        let mut unowned = Spin::new(UNOWNED_SPIN);

        loop {
            if self.try_acquire_write()
                || self.spin_to_acquire(&mut unowned, || self.try_acquire_write())
            {
                self.owner.set_current();
                #[cfg(feature = "stats")]
                self.record_write_acquisitions();
                return;
            }
            listener!(self.writer_event => listener);
            if self.state.load(Ordering::Acquire) != 0 {
                #[cfg(feature = "stats")]
                if !did_block {
                    self.record_write_contentions();
                    did_block = true;
                }
                block_on(listener);
            }
        }
    }

    #[inline]
    fn try_lock_exclusive(&self) -> bool {
        let acquired = self
            .state
            .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            self.owner.set_current();
            #[cfg(feature = "stats")]
            self.record_write_acquisitions();
        }
        acquired
    }

    #[inline]
    unsafe fn unlock_exclusive(&self) {
        // SAFETY: `lock_api` only calls `unlock_exclusive` for the current
        // exclusive owner, so clearing the owner record, the write-lock bit,
        // and waking waiters preserves the raw rwlock state machine.
        self.owner.clear();
        self.state.store(0, Ordering::Release);

        // Wake up all waiting readers and one writer
        self.reader_event.notify(usize::MAX);
        self.writer_event.notify(1);
    }
}

impl RawRwLock {
    /// Retries `try_acquire` while spinning beats sleeping; `false` means block.
    ///
    /// Spins while the recorded writer runs, within [`RUNNING_SPIN`], or while
    /// no writer is recorded, within `unowned`. Stops on a pending reschedule
    /// or a writer that is not running. Uniprocessor builds and non-sleepable
    /// contexts do not spin.
    ///
    /// # Panics
    ///
    /// Panics when the recorded writer is this task, which would otherwise
    /// sleep on an unlock only it can perform.
    #[inline]
    fn spin_to_acquire(&self, unowned: &mut Spin, mut try_acquire: impl FnMut() -> bool) -> bool {
        if self.owner.is_current() {
            Self::panic_already_owns();
        }
        // `is_sleepable_context` also guarantees the current task that
        // `needs_resched` requires.
        if !ktask::can_optimistic_spin() || !ktask::is_sleepable_context() {
            return false;
        }
        let mut running = Spin::new(RUNNING_SPIN);
        while !ktask::needs_resched() {
            let budget = match self.owner.state() {
                LockOwnerState::Running => &mut running,
                LockOwnerState::Unowned => &mut *unowned,
                LockOwnerState::NotRunning => return false,
            };
            if !budget.spin() {
                return false;
            }
            if try_acquire() {
                return true;
            }
        }
        false
    }

    /// `lock_api` has no error return for a same-task write re-entry.
    #[cold]
    #[inline(never)]
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): \
                  lock_api::RawRwLock::lock_exclusive and lock_shared return (), so same-task \
                  write re-entry has no error channel; add a fallible acquire that returns \
                  AlreadyOwned and move this check onto that result"
    )]
    fn panic_already_owns() -> ! {
        panic!(
            "{} tried to acquire rwlock it already owns.",
            ktask::current().id_name()
        );
    }

    /// The relaxed load keeps a held lock from pulling the line exclusive.
    #[inline]
    fn try_acquire_write(&self) -> bool {
        self.state.load(Ordering::Relaxed) == 0
            && self
                .state
                .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    }

    /// Leaves reader overflow to the `lock_shared` loop, which panics on it.
    #[inline]
    fn try_acquire_read(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        state & WRITE_LOCKED == 0
            && state < MAX_READERS
            && self
                .state
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    }
}

#[cfg(feature = "stats")]
impl RawRwLock {
    /// Creates a new [`RawRwLock`] bound to `stats`.
    #[inline(always)]
    pub const fn new_with_stats(stats: &'static klockstat::LockClassStats) -> Self {
        Self {
            state: AtomicU32::new(0),
            owner: LockOwner::new(),
            writer_event: Event::new(),
            reader_event: Event::new(),
            stats,
        }
    }

    #[inline(always)]
    fn record_read_acquisitions(&self) {
        self.stats.record_acquisitions(1);
    }

    #[inline(always)]
    fn record_write_acquisitions(&self) {
        self.stats.record_acquisitions(1);
    }

    #[inline(always)]
    fn record_read_contentions(&self) {
        self.stats.record_contentions(1);
    }

    #[inline(always)]
    fn record_write_contentions(&self) {
        self.stats.record_contentions(1);
    }
}

/// A reader-writer lock built on [`lock_api::RwLock`].
pub struct RwLock<T>(lock_api::RwLock<RawRwLock, T>);

impl<T> core::ops::Deref for RwLock<T> {
    type Target = lock_api::RwLock<RawRwLock, T>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Default> Default for RwLock<T> {
    #[inline(always)]
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> RwLock<T> {
    /// Creates a new [`RwLock`].
    #[inline(always)]
    #[cfg(not(feature = "stats"))]
    pub const fn new(val: T) -> Self {
        Self(lock_api::RwLock::const_new(RawRwLock::new(), val))
    }

    /// Creates a new [`RwLock`] bound to this init site's lock class.
    #[inline(always)]
    #[cfg(feature = "stats")]
    #[track_caller]
    pub fn new(val: T) -> Self {
        let stats = klockstat::class_for_init_site(core::panic::Location::caller(), "RwLock");
        Self::new_with_stats(val, stats)
    }

    /// Creates an [`RwLock`] bound to `stats`.
    #[inline(always)]
    #[cfg(feature = "stats")]
    pub const fn new_with_stats(val: T, stats: &'static klockstat::LockClassStats) -> Self {
        Self(lock_api::RwLock::const_new(
            RawRwLock::new_with_stats(stats),
            val,
        ))
    }

    /// Creates an [`RwLock`] from a custom [`RawRwLock`] and initial value.
    #[inline(always)]
    pub const fn const_new(raw: RawRwLock, val: T) -> Self {
        Self(lock_api::RwLock::const_new(raw, val))
    }
}

/// A read guard for a [`RwLock`].
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwLock, T>;
/// A write guard for a [`RwLock`].
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwLock, T>;
