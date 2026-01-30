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
    _phantom: PhantomData<G>,
    #[cfg(feature = "smp")]
    lock: AtomicBool,
    data: UnsafeCell<T>,
}

/// RAII guard for spinlock.
///
/// Provides mutable access to the protected data and automatically
/// releases the lock when dropped.
pub struct SpinLockGuard<'a, G: BaseGuard, T: ?Sized + 'a> {
    _phantom: &'a PhantomData<G>,
    guard_state: G::State,
    data: *mut T,
    #[cfg(feature = "smp")]
    lock: &'a AtomicBool,
}

// Same unsafe impls as `std::sync::Mutex`
unsafe impl<G: BaseGuard, T: ?Sized + Send> Sync for SpinLock<G, T> {}
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for SpinLock<G, T> {}

impl<G: BaseGuard, T> SpinLock<G, T> {
    /// Create a new spinlock.
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self {
            _phantom: PhantomData,
            data: UnsafeCell::new(data),
            #[cfg(feature = "smp")]
            lock: AtomicBool::new(false),
        }
    }

    /// Consume the lock and return the inner value.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
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
            // Try to acquire using weak CAS in a loop
            while self
                .lock
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                // Spin until lock appears available
                while self.is_locked() {
                    core::hint::spin_loop();
                }
            }
        }

        SpinLockGuard {
            _phantom: &PhantomData,
            guard_state,
            data: unsafe { &mut *self.data.get() },
            #[cfg(feature = "smp")]
            lock: &self.lock,
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
            self.lock.load(Ordering::Relaxed)
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
            .lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();

        #[cfg(not(feature = "smp"))]
        let is_unlocked = true;

        if is_unlocked {
            Some(SpinLockGuard {
                _phantom: &PhantomData,
                guard_state,
                data: unsafe { &mut *self.data.get() },
                #[cfg(feature = "smp")]
                lock: &self.lock,
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
        self.lock.store(false, Ordering::Release);
    }

    /// Get mutable reference (zero-cost).
    ///
    /// Since this requires a mutable reference to the lock itself,
    /// no actual locking is needed.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
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
        unsafe { &*self.data }
    }
}

impl<G: BaseGuard, T: ?Sized> DerefMut for SpinLockGuard<'_, G, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data }
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
        self.lock.store(false, Ordering::Release);

        G::release(self.guard_state);
    }
}

#[cfg(feature = "unittest")]
pub mod tests_lock {
    extern crate alloc;
    
    use super::*;
    use crate::NoOp;
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    type TestSpinLock<T> = SpinLock<NoOp, T>;

    test_fn! {
        using TestResult;
        
        fn test_spinlock_multiple_locks() {
            let lock = TestSpinLock::new(0);
            for i in 0..10 {
                let mut guard = lock.lock();
                *guard += i;
            }
            assert_eq!(*lock.lock(), 45); // 0+1+2+...+9 = 45
        }
    }

    test_fn! {
        using TestResult;
        
        fn test_spinlock_is_locked_after_lock() {
            let lock = TestSpinLock::new(42);
            let _guard = lock.lock();
            cfg_if::cfg_if! {
                if #[cfg(feature = "smp")] {
                    assert!(lock.is_locked());
                } else {
                    // On non-SMP, is_locked always returns false
                    assert!(!lock.is_locked());
                }
            }
        }
    }

    test_fn! {
        using TestResult;
        
        fn test_spinlock_is_locked_after_drop() {
            let lock = TestSpinLock::new(42);
            {
                let _guard = lock.lock();
                // locked here
            }
            // After guard dropped, should be unlocked
            assert!(!lock.is_locked());
        }
    }

    test_fn! {
        using TestResult;
        
        fn test_spinlock_complex_state_mutation() {
            use alloc::string::String;
            let lock = TestSpinLock::new(String::new());
            {
                let mut guard = lock.lock();
                guard.push_str("hello");
            }
            {
                let mut guard = lock.lock();
                guard.push_str(" world");
            }
            assert_eq!(*lock.lock(), "hello world");
        }
    }

    test_fn! {
        using TestResult;
        
        fn test_spinlock_try_lock_after_explicit_drop() {
            let lock = TestSpinLock::new(0);
            let guard = lock.lock();
            drop(guard);
            
            // Should be able to try_lock after explicit drop
            let result = lock.try_lock();
            assert!(result.is_some());
        }
    }

    tests_name!(TEST_SPINLOCK;
        test_spinlock_multiple_locks,
        test_spinlock_is_locked_after_lock,
        test_spinlock_is_locked_after_drop,
        test_spinlock_complex_state_mutation,
        test_spinlock_try_lock_after_explicit_drop,
    );
}
