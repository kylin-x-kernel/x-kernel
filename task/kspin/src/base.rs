// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A naïve spinning mutex.
//!
//! Waiting threads hammer an atomic variable until it becomes available. Best-case latency is low, but worst-case
//! latency is theoretically infinite.
//!
//! Based on [`spin::Mutex`](https://docs.rs/spin/latest/src/spin/mutex/spin.rs.html).

#[cfg(feature = "smp")]
use core::sync::atomic::{AtomicBool, Ordering};
use core::{
    cell::UnsafeCell,
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::guard::BaseGuard;

/// A [spin lock](https://en.m.wikipedia.org/wiki/Spinlock) providing mutually
/// exclusive access to data.
///
/// This is a base struct, the specific behavior depends on the generic
/// parameter `G` that implements [`BaseGuard`], such as whether to disable
/// local IRQs or kernel preemption before acquiring the lock.
///
/// For single-core environment (without the "smp" feature), we remove the lock
/// state, CPU can always get the lock if we follow the proper guard in use.
pub struct BaseSpinLock<G: BaseGuard, T: ?Sized> {
    guard_kind: PhantomData<G>,
    #[cfg(feature = "smp")]
    busy_flag: AtomicBool,
    storage: UnsafeCell<T>,
}

/// A guard that provides mutable data access.
///
/// When the guard falls out of scope it will release the lock.
pub struct BaseSpinLockGuard<'a, G: BaseGuard, T: ?Sized + 'a> {
    guard_type: &'a PhantomData<G>,
    irq_token: G::State,
    slot: *mut T,
    #[cfg(feature = "smp")]
    flag_ref: &'a AtomicBool,
}

// Same unsafe impls as `std::sync::Mutex`
unsafe impl<G: BaseGuard, T: ?Sized + Send> Sync for BaseSpinLock<G, T> {}
unsafe impl<G: BaseGuard, T: ?Sized + Send> Send for BaseSpinLock<G, T> {}

impl<G: BaseGuard, T> BaseSpinLock<G, T> {
    /// Creates a new [`BaseSpinLock`] wrapping the supplied data.
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self {
            guard_kind: PhantomData,
            storage: UnsafeCell::new(data),
            #[cfg(feature = "smp")]
            busy_flag: AtomicBool::new(false),
        }
    }

    /// Consumes this [`BaseSpinLock`] and unwraps the underlying data.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        // We know statically that there are no outstanding references to
        // `self` so there's no need to lock.
        let BaseSpinLock { storage, .. } = self;
        storage.into_inner()
    }
}

impl<G: BaseGuard, T: ?Sized> BaseSpinLock<G, T> {
    /// Locks the [`BaseSpinLock`] and returns a guard that permits access to the inner data.
    ///
    /// The returned value may be dereferenced for data access
    /// and the lock will be dropped when the guard falls out of scope.
    #[inline(always)]
    pub fn lock(&self) -> BaseSpinLockGuard<'_, G, T> {
        let irq_state = G::acquire();
        #[cfg(feature = "smp")]
        {
            // Fast path: optimistic attempt; if it fails, spin until the flag
            // becomes available again.
            loop {
                if self
                    .busy_flag
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
        BaseSpinLockGuard {
            guard_type: &PhantomData,
            irq_token: irq_state,
            slot: unsafe { &mut *self.storage.get() },
            #[cfg(feature = "smp")]
            flag_ref: &self.busy_flag,
        }
    }

    /// Returns `true` if the lock is currently held.
    ///
    /// # Safety
    ///
    /// This function provides no synchronization guarantees and so its result should be considered 'out of date'
    /// the instant it is called. Do not use it for synchronization purposes. However, it may be useful as a heuristic.
    #[inline(always)]
    pub fn is_locked(&self) -> bool {
        cfg_if::cfg_if! {
            if #[cfg(feature = "smp")] {
                self.busy_flag.load(Ordering::Relaxed)
            } else {
                false
            }
        }
    }

    /// Try to lock this [`BaseSpinLock`], returning a lock guard if successful.
    #[inline(always)]
    pub fn try_lock(&self) -> Option<BaseSpinLockGuard<'_, G, T>> {
        let irq_state = G::acquire();

        cfg_if::cfg_if! {
            if #[cfg(feature = "smp")] {
                // Strong CAS avoids spurious failures in the contended fast-path.
                let is_unlocked = self
                    .busy_flag
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok();
            } else {
                let is_unlocked = true;
            }
        }

        if is_unlocked {
            Some(BaseSpinLockGuard {
                guard_type: &PhantomData,
                irq_token: irq_state,
                slot: unsafe { &mut *self.storage.get() },
                #[cfg(feature = "smp")]
                flag_ref: &self.busy_flag,
            })
        } else {
            G::release(irq_state);
            None
        }
    }

    /// Force unlock this [`BaseSpinLock`].
    ///
    /// # Safety
    ///
    /// This is *extremely* unsafe if the lock is not held by the current
    /// thread. However, this can be useful in some instances for exposing the
    /// lock to FFI that doesn't know how to deal with RAII.
    #[inline(always)]
    pub unsafe fn force_unlock(&self) {
        #[cfg(feature = "smp")]
        self.busy_flag.store(false, Ordering::Release);
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the [`BaseSpinLock`] mutably, and a mutable reference is guaranteed to be exclusive in
    /// Rust, no actual locking needs to take place -- the mutable borrow statically guarantees no locks exist. As
    /// such, this is a 'zero-cost' operation.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        // We know statically that there are no other references to `self`, so
        // there's no need to lock the inner mutex.
        unsafe { &mut *self.storage.get() }
    }
}

impl<G: BaseGuard, T: Default> Default for BaseSpinLock<G, T> {
    #[inline(always)]
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for BaseSpinLock<G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => write!(f, "SpinLock {{ data: ")
                .and_then(|()| (*guard).fmt(f))
                .and_then(|()| write!(f, "}}")),
            None => write!(f, "SpinLock {{ <locked> }}"),
        }
    }
}

impl<G: BaseGuard, T: ?Sized> Deref for BaseSpinLockGuard<'_, G, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // We know statically that only we are referencing data
        unsafe { &*self.slot }
    }
}

impl<G: BaseGuard, T: ?Sized> DerefMut for BaseSpinLockGuard<'_, G, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        // We know statically that only we are referencing data
        unsafe { &mut *self.slot }
    }
}

impl<G: BaseGuard, T: ?Sized + fmt::Debug> fmt::Debug for BaseSpinLockGuard<'_, G, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<G: BaseGuard, T: ?Sized> Drop for BaseSpinLockGuard<'_, G, T> {
    /// The dropping of the [`BaseSpinLockGuard`] will release the lock it was
    /// created from.
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(feature = "smp")]
        self.flag_ref.store(false, Ordering::Release);
        G::release(self.irq_token);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc::channel,
        },
        thread,
    };

    use super::*;

    struct TestGuardIrq;

    static mut IRQ_CNT: u32 = 0;
    impl BaseGuard for TestGuardIrq {
        type State = u32;

        fn acquire() -> Self::State {
            unsafe {
                IRQ_CNT += 1;
                IRQ_CNT
            }
        }

        fn release(_: Self::State) {
            unsafe {
                IRQ_CNT -= 1;
            }
        }
    }

    type TestSpinIrq<T> = BaseSpinLock<TestGuardIrq, T>;
    type SpinMutex<T> = crate::SpinRaw<T>;

    #[derive(Eq, PartialEq, Debug)]
    struct NonCopy(i32);

    #[test]
    fn basic_lock_unlock() {
        let simple_lock = SpinMutex::<_>::new(());
        drop(simple_lock.lock());
        drop(simple_lock.lock());
    }

    #[test]
    #[cfg(feature = "smp")]
    fn lots_and_lots() {
        static GLOBAL_MUTEX: SpinMutex<()> = SpinMutex::<_>::new(());
        static mut COUNTER: u32 = 0;
        const INNER_ITERS: u32 = 1000;
        const THREAD_PAIRS: u32 = 3;

        fn bump_shared_counter() {
            for _ in 0..INNER_ITERS {
                unsafe {
                    let guard = GLOBAL_MUTEX.lock();
                    COUNTER += 1;
                    core::mem::drop(guard);
                }
            }
        }

        let (sender, receiver) = channel();
        let mut worker_handles = Vec::new();
        for _ in 0..THREAD_PAIRS {
            let notifier1 = sender.clone();
            worker_handles.push(thread::spawn(move || {
                bump_shared_counter();
                notifier1.send(()).unwrap();
            }));
            let notifier2 = sender.clone();
            worker_handles.push(thread::spawn(move || {
                bump_shared_counter();
                notifier2.send(()).unwrap();
            }));
        }

        drop(sender);
        for _ in 0..(2 * THREAD_PAIRS) {
            receiver.recv().unwrap();
        }
        assert_eq!(unsafe { COUNTER }, INNER_ITERS * THREAD_PAIRS * 2);

        for handle in worker_handles {
            handle.join().unwrap();
        }
    }

    #[test]
    #[cfg(feature = "smp")]
    fn try_lock() {
        let guarded_value = SpinMutex::<_>::new(42);

        // First attempt should succeed
        let first = guarded_value.try_lock();
        assert_eq!(first.as_ref().map(|r| **r), Some(42));

        // Second simultaneous attempt must fail
        let second = guarded_value.try_lock();
        assert!(second.is_none());

        // After releasing the first guard, a new attempt should succeed
        ::core::mem::drop(first);
        let third = guarded_value.try_lock();
        assert_eq!(third.as_ref().map(|r| **r), Some(42));
    }

    #[test]
    fn test_irq_lock_restored() {
        let irq_lock = TestSpinIrq::new(());
        let guard = irq_lock.lock();
        assert_eq!(unsafe { IRQ_CNT }, 1);
        ::core::mem::drop(guard);
        assert_eq!(unsafe { IRQ_CNT }, 0);
    }

    #[test]
    #[cfg(feature = "smp")]
    fn test_irq_try_lock_failed() {
        let irq_guarded = TestSpinIrq::new(());
        let primary = irq_guarded.lock();
        assert_eq!(unsafe { IRQ_CNT }, 1);
        let competing = irq_guarded.try_lock();
        assert!(competing.is_none());
        assert_eq!(unsafe { IRQ_CNT }, 1);
        drop(primary);
    }

    #[test]
    fn test_into_inner() {
        let wrapper = SpinMutex::<_>::new(NonCopy(10));
        assert_eq!(wrapper.into_inner(), NonCopy(10));
    }

    #[test]
    fn test_into_inner_drop() {
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mutex = SpinMutex::<_>::new(DropCounter(drops.clone()));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        {
            let inner = mutex.into_inner();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            core::mem::drop(inner);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_mutex_arc_nested() {
        // Exercise nested spin locks behind `Arc` and validate access to inner data.
        let outer = Arc::new(SpinMutex::<_>::new(1));
        let nested = Arc::new(SpinMutex::<_>::new(outer));
        let (sender, receiver) = channel();
        let worker = thread::spawn(move || {
            let first_guard = nested.lock();
            let second_guard = first_guard.lock();
            assert_eq!(*second_guard, 1);
            sender.send(()).unwrap();
        });
        receiver.recv().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn test_mutex_arc_access_in_unwind() {
        let shared = Arc::new(SpinMutex::<_>::new(1));
        let captured = shared.clone();
        let _ = thread::spawn(move || {
            struct Unwinder {
                handle: Arc<SpinMutex<i32>>,
            }
            impl Drop for Unwinder {
                fn drop(&mut self) {
                    *self.handle.lock() += 1;
                }
            }
            let _scope = Unwinder { handle: captured };
            panic!();
        })
        .join();
        let final_guard = shared.lock();
        assert_eq!(*final_guard, 2);
    }

    #[test]
    fn test_mutex_unsized() {
        let slice_mutex: &SpinMutex<[i32]> = &SpinMutex::<_>::new([1, 2, 3]);
        {
            let slice = &mut *slice_mutex.lock();
            slice[0] = 4;
            slice[2] = 5;
        }
        let expected: &[i32] = &[4, 2, 5];
        assert_eq!(&*slice_mutex.lock(), expected);
    }

    #[test]
    fn test_mutex_force_lock() {
        let raw_lock = SpinMutex::<_>::new(());
        ::std::mem::forget(raw_lock.lock());
        unsafe {
            raw_lock.force_unlock();
        }
        assert!(raw_lock.try_lock().is_some());
    }
}
