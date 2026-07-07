// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A reader-writer lock implementation.

use core::sync::atomic::{AtomicU32, Ordering};

use event_listener::{Event, listener};
use ktask::future::block_on;

const WRITE_LOCKED: u32 = 1 << 31;
const MAX_READERS: u32 = WRITE_LOCKED - 1;

/// A [`lock_api::RawRwLock`] implementation.
///
/// Allows multiple readers or a single writer.
/// The high bit of the state represents the write lock,
/// and the low 31 bits represent the reader count.
pub struct RawRwLock {
    state: AtomicU32, // High bit: write lock, low 31 bits: reader count
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

    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = RawRwLock::new();

    #[inline]
    fn lock_shared(&self) {
        #[cfg(feature = "stats")]
        let mut did_block = false;

        loop {
            let state = self.state.load(Ordering::Relaxed);

            // Check if write locked
            if state & WRITE_LOCKED != 0 {
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

        loop {
            // Try to acquire write lock
            match self
                .state
                .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    #[cfg(feature = "stats")]
                    self.record_write_acquisitions();
                    return;
                }
                Err(_) => {
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
        }
    }

    #[inline]
    fn try_lock_exclusive(&self) -> bool {
        let acquired = self
            .state
            .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            #[cfg(feature = "stats")]
            self.record_write_acquisitions();
        }
        acquired
    }

    #[inline]
    unsafe fn unlock_exclusive(&self) {
        // SAFETY: `lock_api` only calls `unlock_exclusive` for the current
        // exclusive owner, so clearing the write-lock bit and waking waiters
        // preserves the raw rwlock state machine.
        self.state.store(0, Ordering::Release);

        // Wake up all waiting readers and one writer
        self.reader_event.notify(usize::MAX);
        self.writer_event.notify(1);
    }
}

#[cfg(feature = "stats")]
impl RawRwLock {
    /// Creates a new [`RawRwLock`] bound to `stats`.
    #[inline(always)]
    pub const fn new_with_stats(stats: &'static klockstat::LockClassStats) -> Self {
        Self {
            state: AtomicU32::new(0),
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
