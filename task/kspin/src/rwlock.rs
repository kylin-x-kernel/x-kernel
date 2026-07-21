// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ/preemption-aware spinning reader-writer lock.

use core::{
    cell::UnsafeCell,
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::guard::BaseGuard;

const WRITE_LOCKED: usize = 1usize << (usize::BITS - 1);
const READER_MASK: usize = WRITE_LOCKED - 1;

/// A spinning reader-writer lock with configurable guard behavior.
///
/// The guard type `G` controls the execution context while the lock is held.
/// For example, [`crate::SpinRwNoIrq`] disables preemption and IRQs for both
/// readers and writers, making it suitable for short kernel paths that must not
/// sleep.
pub struct SpinRwLock<G: BaseGuard, T: ?Sized> {
    marker: PhantomData<G>,
    state: AtomicUsize,
    #[cfg(feature = "stats")]
    stats: &'static klockstat::LockClassStats,
    storage: UnsafeCell<T>,
}

/// RAII read guard for [`SpinRwLock`].
pub struct SpinRwLockReadGuard<'a, G: BaseGuard, T: ?Sized + 'a> {
    lock: &'a SpinRwLock<G, T>,
    guard_state: G::State,
}

/// RAII write guard for [`SpinRwLock`].
pub struct SpinRwLockWriteGuard<'a, G: BaseGuard, T: ?Sized + 'a> {
    lock: &'a SpinRwLock<G, T>,
    guard_state: G::State,
}

// SAFETY: `SpinRwLock` allows shared references to `T` while one or more read
// guards hold the lock, and unique mutable access only while a write guard
// holds the lock. Sharing the lock is sound when `T` can be shared across
// threads.
unsafe impl<G: BaseGuard, T: ?Sized + Send + Sync> Sync for SpinRwLock<G, T> {}
// SAFETY: moving the lock transfers ownership of the protected storage; access
// remains mediated by the same read/write guard protocol.
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for SpinRwLock<G, T> {}

impl<G: BaseGuard, T> SpinRwLock<G, T> {
    /// Creates a new spinning reader-writer lock.
    #[inline(always)]
    #[cfg(not(feature = "stats"))]
    pub const fn new(data: T) -> Self {
        Self {
            marker: PhantomData,
            state: AtomicUsize::new(0),
            storage: UnsafeCell::new(data),
        }
    }

    /// Creates a new spinning reader-writer lock.
    #[inline(always)]
    #[cfg(feature = "stats")]
    pub const fn new(data: T) -> Self {
        Self::new_with_stats(data, &klockstat::NOOP_CLASS)
    }

    /// Creates a new spinning reader-writer lock bound to `stats`.
    #[inline(always)]
    #[cfg(feature = "stats")]
    pub const fn new_with_stats(data: T, stats: &'static klockstat::LockClassStats) -> Self {
        Self {
            marker: PhantomData,
            state: AtomicUsize::new(0),
            stats,
            storage: UnsafeCell::new(data),
        }
    }

    /// Consumes the lock and returns the protected value.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.storage.into_inner()
    }
}

impl<G: BaseGuard, T: ?Sized> SpinRwLock<G, T> {
    /// Acquires a shared read lock.
    #[inline(always)]
    pub fn read(&self) -> SpinRwLockReadGuard<'_, G, T> {
        let guard_state = G::acquire();
        #[cfg(feature = "stats")]
        let mut did_wait = false;

        loop {
            let state = self.state.load(Ordering::Acquire);
            if state & WRITE_LOCKED != 0 {
                #[cfg(feature = "stats")]
                {
                    did_wait = true;
                }
                core::hint::spin_loop();
                continue;
            }
            if state == READER_MASK {
                panic!("too many spin rwlock readers");
            }
            if self
                .state
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                #[cfg(feature = "stats")]
                self.record_read_acquisition(did_wait);
                return SpinRwLockReadGuard {
                    lock: self,
                    guard_state,
                };
            }
        }
    }

    /// Tries to acquire a shared read lock without spinning.
    #[inline(always)]
    pub fn try_read(&self) -> Option<SpinRwLockReadGuard<'_, G, T>> {
        let guard_state = G::acquire();
        let state = self.state.load(Ordering::Relaxed);
        let acquired = state & WRITE_LOCKED == 0
            && state != READER_MASK
            && self
                .state
                .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok();

        if acquired {
            #[cfg(feature = "stats")]
            self.record_read_acquisition(false);
            Some(SpinRwLockReadGuard {
                lock: self,
                guard_state,
            })
        } else {
            G::release(guard_state);
            None
        }
    }

    /// Acquires an exclusive write lock.
    #[inline(always)]
    pub fn write(&self) -> SpinRwLockWriteGuard<'_, G, T> {
        let guard_state = G::acquire();
        #[cfg(feature = "stats")]
        let mut did_wait = false;

        while self
            .state
            .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            #[cfg(feature = "stats")]
            {
                did_wait = true;
            }
            core::hint::spin_loop();
        }

        #[cfg(feature = "stats")]
        self.record_write_acquisition(did_wait);
        SpinRwLockWriteGuard {
            lock: self,
            guard_state,
        }
    }

    /// Tries to acquire an exclusive write lock without spinning.
    #[inline(always)]
    pub fn try_write(&self) -> Option<SpinRwLockWriteGuard<'_, G, T>> {
        let guard_state = G::acquire();
        let acquired = self
            .state
            .compare_exchange(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();

        if acquired {
            #[cfg(feature = "stats")]
            self.record_write_acquisition(false);
            Some(SpinRwLockWriteGuard {
                lock: self,
                guard_state,
            })
        } else {
            G::release(guard_state);
            None
        }
    }

    /// Gets mutable access without locking.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` proves no other references to the lock exist, so
        // no guard can concurrently access the protected storage.
        unsafe { &mut *self.storage.get() }
    }
}

impl<G: BaseGuard, T: Default> Default for SpinRwLock<G, T> {
    #[inline(always)]
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for SpinRwLock<G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_read() {
            Some(guard) => f
                .debug_struct("SpinRwLock")
                .field("data", &&*guard)
                .finish(),
            None => f
                .debug_struct("SpinRwLock")
                .field("data", &"<locked>")
                .finish(),
        }
    }
}

impl<G: BaseGuard, T: ?Sized> Deref for SpinRwLockReadGuard<'_, G, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: the read guard owns a shared lock; shared references derived
        // from it cannot outlive the guard.
        unsafe { &*self.lock.storage.get() }
    }
}

impl<G: BaseGuard, T: ?Sized> Deref for SpinRwLockWriteGuard<'_, G, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: the write guard owns the exclusive lock; shared references
        // derived from it cannot outlive the guard.
        unsafe { &*self.lock.storage.get() }
    }
}

impl<G: BaseGuard, T: ?Sized> DerefMut for SpinRwLockWriteGuard<'_, G, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` proves this guard is borrowed exclusively, and
        // the guard owns the exclusive lock until drop.
        unsafe { &mut *self.lock.storage.get() }
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for SpinRwLockReadGuard<'_, G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for SpinRwLockWriteGuard<'_, G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<G: BaseGuard, T: ?Sized> Drop for SpinRwLockReadGuard<'_, G, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.lock.state.fetch_sub(1, Ordering::Release);
        G::release(self.guard_state);
    }
}

impl<G: BaseGuard, T: ?Sized> Drop for SpinRwLockWriteGuard<'_, G, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.lock.state.store(0, Ordering::Release);
        G::release(self.guard_state);
    }
}

#[cfg(feature = "stats")]
impl<G: BaseGuard, T: ?Sized> SpinRwLock<G, T> {
    #[inline(always)]
    fn record_read_acquisition(&self, did_wait: bool) {
        self.stats.record_acquisitions(1);
        if did_wait {
            self.stats.record_contentions(1);
        }
    }

    #[inline(always)]
    fn record_write_acquisition(&self, did_wait: bool) {
        self.stats.record_acquisitions(1);
        if did_wait {
            self.stats.record_contentions(1);
        }
    }
}
