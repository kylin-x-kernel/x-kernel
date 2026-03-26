// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Spinlock implementation with configurable guards.
//!
//! This module provides a generic spinlock that can be configured
//! with different guard types to control preemption and interrupts.

#[cfg(feature = "smp")]
use core::sync::atomic::{AtomicBool, Ordering};
use core::{
    cell::UnsafeCell,
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::guard::BaseGuard;

/// A spinlock with configurable guard behavior.
///
/// The guard type `G` determines what happens when acquiring the lock:
/// - [`crate::NoOp`]: No special behavior (fastest, least safe)
/// - [`crate::NoPreempt`]: Disables preemption
/// - [`crate::IrqSave`]: Saves and disables IRQs
/// - [`crate::NoPreemptIrqSave`]: Disables both preemption and IRQs (safest)
///
/// # Single-core optimization
///
/// Without the `smp` feature, the lock state is optimized away since
/// no actual atomic synchronization is needed.
///
/// # Examples
///
/// ```rust,ignore
/// use kspin::SpinNoIrq;
///
/// let lock = SpinNoIrq::new(42);
/// {
///     let guard = lock.lock();
///     assert_eq!(*guard, 42);
///     // Preemption and IRQs are disabled here
/// } // Lock released, IRQs and preemption restored
/// ```
pub struct SpinLock<G: BaseGuard, T: ?Sized> {
    marker: PhantomData<G>,
    #[cfg(feature = "smp")]
    flag: AtomicBool,
    storage: UnsafeCell<T>,
}

/// RAII guard for spinlock.
///
/// Provides mutable access to the protected data and automatically
/// releases the lock when dropped.
pub struct SpinLockGuard<'a, G: BaseGuard, T: ?Sized + 'a> {
    _token: &'a PhantomData<G>,
    guard_state: G::State,
    ptr: *mut T,
    #[cfg(feature = "smp")]
    flag_ref: &'a AtomicBool,
}

// Same unsafe impls as `std::sync::Mutex`
unsafe impl<G: BaseGuard, T: ?Sized + Send> Sync for SpinLock<G, T> {}
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for SpinLock<G, T> {}

impl<G: BaseGuard, T> SpinLock<G, T> {
    /// Create a new spinlock.
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self {
            marker: PhantomData,
            storage: UnsafeCell::new(data),
            #[cfg(feature = "smp")]
            flag: AtomicBool::new(false),
        }
    }

    /// Consume the lock and return the inner value.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.storage.into_inner()
    }
}

impl<G: BaseGuard, T: ?Sized> SpinLock<G, T> {
    /// Acquire the lock, blocking until available.
    ///
    /// # Panics
    ///
    /// May panic or deadlock if called while already holding the lock.
    #[inline(always)]
    pub fn lock(&self) -> SpinLockGuard<'_, G, T> {
        let guard_state = G::acquire();

        #[cfg(feature = "smp")]
        {
            // Opportunistic acquire: weak CAS in a loop, with a secondary
            // spin phase while the lock appears taken.
            loop {
                if self
                    .flag
                    .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
                while self.is_locked() {
                    core::hint::spin_loop();
                }
            }
        }

        SpinLockGuard {
            _token: &PhantomData,
            guard_state,
            ptr: unsafe { &mut *self.storage.get() },
            #[cfg(feature = "smp")]
            flag_ref: &self.flag,
        }
    }

    /// Check if lock is currently held (heuristic only).
    ///
    /// # Warning
    ///
    /// This provides no synchronization guarantees. The result
    /// may be stale immediately. Do not use for synchronization.
    #[inline(always)]
    pub fn is_locked(&self) -> bool {
        #[cfg(feature = "smp")]
        {
            self.flag.load(Ordering::Relaxed)
        }
        #[cfg(not(feature = "smp"))]
        {
            false
        }
    }

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `Some(guard)` if successful, `None` if already locked.
    #[inline(always)]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, G, T>> {
        let guard_state = G::acquire();

        #[cfg(feature = "smp")]
        let is_unlocked = self
            .flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();

        #[cfg(not(feature = "smp"))]
        let is_unlocked = true;

        if is_unlocked {
            Some(SpinLockGuard {
                _token: &PhantomData,
                guard_state,
                ptr: unsafe { &mut *self.storage.get() },
                #[cfg(feature = "smp")]
                flag_ref: &self.flag,
            })
        } else {
            G::release(guard_state);
            None
        }
    }

    /// Force unlock (unsafe).
    ///
    /// # Safety
    ///
    /// Must only be called if the current thread holds the lock.
    /// Violating this may cause data races.
    #[inline(always)]
    pub unsafe fn force_unlock(&self) {
        #[cfg(feature = "smp")]
        self.flag.store(false, Ordering::Release);
    }

    /// Get mutable reference (zero-cost).
    ///
    /// Since this requires a mutable reference to the lock itself,
    /// no actual locking is needed.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.storage.get() }
    }
}

impl<G: BaseGuard, T: Default> Default for SpinLock<G, T> {
    #[inline(always)]
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("SpinLock").field("data", &&*guard).finish(),
            None => f
                .debug_struct("SpinLock")
                .field("data", &"<locked>")
                .finish(),
        }
    }
}

impl<G: BaseGuard, T: ?Sized> Deref for SpinLockGuard<'_, G, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

impl<G: BaseGuard, T: ?Sized> DerefMut for SpinLockGuard<'_, G, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.ptr }
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for SpinLockGuard<'_, G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<G: BaseGuard, T: ?Sized> Drop for SpinLockGuard<'_, G, T> {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(feature = "smp")]
        self.flag_ref.store(false, Ordering::Release);

        G::release(self.guard_state);
    }
}
