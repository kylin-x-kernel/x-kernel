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
}

impl RawRwLock {
    /// Creates a new [`RawRwLock`].
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            writer_event: Event::new(),
            reader_event: Event::new(),
        }
    }
}

impl Default for RawRwLock {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl lock_api::RawRwLock for RawRwLock {
    type GuardMarker = lock_api::GuardSend;

    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = RawRwLock::new();

    #[inline]
    fn lock_shared(&self) {
        loop {
            let state = self.state.load(Ordering::Relaxed);

            // Check if write locked
            if state & WRITE_LOCKED != 0 {
                listener!(self.reader_event => listener);
                if self.state.load(Ordering::Acquire) & WRITE_LOCKED != 0 {
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
                Ok(_) => return,
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
        self.state
            .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    unsafe fn unlock_shared(&self) {
        let state = self.state.fetch_sub(1, Ordering::Release);

        // Wake up a waiting writer if this was the last reader
        if state == 1 {
            self.writer_event.notify(1);
        }
    }

    #[inline]
    fn lock_exclusive(&self) {
        loop {
            // Try to acquire write lock
            match self
                .state
                .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(_) => {
                    listener!(self.writer_event => listener);
                    if self.state.load(Ordering::Acquire) != 0 {
                        block_on(listener);
                    }
                }
            }
        }
    }

    #[inline]
    fn try_lock_exclusive(&self) -> bool {
        self.state
            .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    unsafe fn unlock_exclusive(&self) {
        self.state.store(0, Ordering::Release);

        // Wake up all waiting readers and one writer
        self.reader_event.notify(usize::MAX);
        self.writer_event.notify(1);
    }
}

/// A reader-writer lock.
pub type RwLock<T> = lock_api::RwLock<RawRwLock, T>;
/// A read guard for a [`RwLock`].
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwLock, T>;
/// A write guard for a [`RwLock`].
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwLock, T>;
