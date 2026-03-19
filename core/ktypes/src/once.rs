// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronization primitives for one-time evaluation.

use core::{
    cell::UnsafeCell,
    fmt,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

/// A primitive that provides lazy one-time initialization.
///
/// Unlike its `std::sync` equivalent, this is generalized such that the closure returns a
/// value to be stored by the [`Once`] (`std::sync::Once` can be trivially emulated with
/// `Once`).
///
/// Because [`Once::new`] is `const`, this primitive may be used to safely initialize statics.
///
/// # Examples
///
/// ```
/// use ktypes;
///
/// static START: ktypes::Once = ktypes::Once::new();
///
/// START.call_once(|| {
///     // run initialization here
/// });
/// ```
pub struct Once<T = ()> {
    /// Atomic state tracking initialization progress
    state: AtomicStatus,
    /// Internal storage for the lazy-initialized value
    storage: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Build debug representation based on current state
        let mut formatter = f.debug_tuple("OnceCell");

        let formatter = match self.get() {
            Some(value) => formatter.field(&value),
            None => formatter.field(&format_args!("<not-ready>")),
        };
        formatter.finish()
    }
}

// Same unsafe impls as `std::sync::RwLock`, because this also allows for
// concurrent reads.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
unsafe impl<T: Send> Send for Once<T> {}

mod status {
    use super::*;

    // SAFETY: This structure has an invariant, namely that the inner atomic u8 must *always* have
    // a value for which there exists a valid Status. This means that users of this API must only
    // be allowed to load and store `Status`es.
    #[repr(transparent)]
    pub struct AtomicStatus(AtomicU8);

    // Four possible states for the Once cell, encoded in the atomic u8 storage.
    // These represent the lifecycle: uninitialized -> executing -> done/failed
    #[repr(u8)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Status {
        Uninitialized = 0x00,
        Initializing  = 0x01,
        Ready         = 0x02,
        Failed        = 0x03,
    }
    impl Status {
        // Construct a status from an inner u8 integer.
        //
        // # Safety
        //
        // For this to be safe, the inner number must have a valid corresponding enum variant.
        unsafe fn new_unchecked(inner: u8) -> Self {
            unsafe { core::mem::transmute(inner) }
        }
    }

    impl AtomicStatus {
        #[inline(always)]
        pub const fn new(status: Status) -> Self {
            // Convert Status enum to underlying u8 representation
            Self(AtomicU8::new(status as u8))
        }

        #[inline(always)]
        pub fn load(&self, ordering: Ordering) -> Status {
            // The invariant ensures the loaded u8 is a valid Status variant
            let raw_value = self.0.load(ordering);
            unsafe { Status::new_unchecked(raw_value) }
        }

        #[inline(always)]
        pub fn store(&self, status: Status, ordering: Ordering) {
            // Store the enum discriminant value directly
            let raw_value = status as u8;
            self.0.store(raw_value, ordering);
        }

        #[inline(always)]
        pub fn compare_exchange(
            &self,
            old: Status,
            new: Status,
            success: Ordering,
            failure: Ordering,
        ) -> Result<Status, Status> {
            // Try to atomically swap old state with new state
            let old_raw = old as u8;
            let new_raw = new as u8;
            let result = self.0.compare_exchange(old_raw, new_raw, success, failure);

            // Convert result values back to Status enum
            match result {
                Ok(current) => Ok(unsafe { Status::new_unchecked(current) }),
                Err(actual) => Err(unsafe { Status::new_unchecked(actual) }),
            }
        }

        #[inline(always)]
        pub fn get_mut(&mut self) -> &mut Status {
            // Direct mutable access is safe with exclusive reference
            let ptr = self.0.get_mut() as *mut u8;
            unsafe { &mut *(ptr.cast::<Status>()) }
        }
    }
}
use self::status::{AtomicStatus, Status};

impl<T> Once<T> {
    /// Performs an initialization routine once and only once. The given closure
    /// will be executed if this is the first time `call_once` has been called,
    /// and otherwise the routine will *not* be invoked.
    ///
    /// This method will block the calling thread if another initialization
    /// routine is currently running.
    ///
    /// When this function returns, it is guaranteed that some initialization
    /// has run and completed (it may not be the closure specified). The
    /// returned pointer will point to the result from the closure that was
    /// run.
    ///
    /// # Panics
    ///
    /// This function will panic if the [`Once`] previously panicked while attempting
    /// to initialize. This is similar to the poisoning behaviour of `std::sync`'s
    /// primitives.
    ///
    /// # Examples
    ///
    /// ```
    /// use ktypes;
    ///
    /// static INIT: ktypes::Once<usize> = ktypes::Once::new();
    ///
    /// fn get_cached_val() -> usize {
    ///     *INIT.call_once(expensive_computation)
    /// }
    ///
    /// fn expensive_computation() -> usize {
    ///     // ...
    /// # 2
    /// }
    /// ```
    pub fn call_once<F: FnOnce() -> T>(&self, f: F) -> &T {
        match self.try_call_once(|| Ok::<T, core::convert::Infallible>(f())) {
            Ok(x) => x,
            Err(void) => match void {},
        }
    }

    /// This method is similar to `call_once`, but allows the given closure to
    /// fail, and lets the `Once` in a uninitialized state if it does.
    ///
    /// This method will block the calling thread if another initialization
    /// routine is currently running.
    ///
    /// When this function returns without error, it is guaranteed that some
    /// initialization has run and completed (it may not be the closure
    /// specified). The returned reference will point to the result from the
    /// closure that was run.
    ///
    /// # Panics
    ///
    /// This function will panic if the [`Once`] previously panicked while attempting
    /// to initialize. This is similar to the poisoning behaviour of `std::sync`'s
    /// primitives.
    ///
    /// # Examples
    ///
    /// ```
    /// use ktypes;
    ///
    /// static INIT: ktypes::Once<usize> = ktypes::Once::new();
    ///
    /// fn get_cached_val() -> Result<usize, String> {
    ///     INIT.try_call_once(expensive_fallible_computation)
    ///         .map(|x| *x)
    /// }
    ///
    /// fn expensive_fallible_computation() -> Result<usize, String> {
    ///     // ...
    /// # Ok(2)
    /// }
    /// ```
    pub fn try_call_once<F: FnOnce() -> Result<T, E>, E>(&self, f: F) -> Result<&T, E> {
        // Fast path: check if already initialized
        if let Some(existing_value) = self.get() {
            return Ok(existing_value);
        }

        // Slow path: perform initialization
        self.try_call_once_slow(f)
    }

    #[cold]
    fn try_call_once_slow<F: FnOnce() -> Result<T, E>, E>(&self, f: F) -> Result<&T, E> {
        loop {
            // Attempt to transition from uninitialized to initializing state
            let exchange_result = self.state.compare_exchange(
                Status::Uninitialized,
                Status::Initializing,
                Ordering::Acquire,
                Ordering::Acquire,
            );

            match exchange_result {
                Ok(_) => {
                    // Successfully claimed initialization responsibility
                    // Implementation continues below
                }
                Err(Status::Failed) => {
                    panic!("OnceCell has failed during a previous initialization attempt")
                }
                Err(Status::Initializing) => {
                    // Another thread is initializing, wait for completion
                    match self.poll() {
                        Some(completed_value) => return Ok(completed_value),
                        None => continue,
                    }
                }
                Err(Status::Ready) => {
                    // Another thread completed initialization
                    return Ok(unsafe {
                        // SAFETY: Status is Ready, so value is initialized
                        self.get_value_unchecked()
                    });
                }
                Err(Status::Uninitialized) => {
                    // CAS failed spuriously, retry
                    continue;
                }
            }

            // We own the initialization process now
            // Set up panic guard to mark cell as failed on panic
            let panic_guard = PanicGuard { state: &self.state };

            // Execute the initialization function
            let initialized_value = match f() {
                Ok(result) => result,
                Err(error) => {
                    // Initialization failed, reset to uninitialized
                    core::mem::forget(panic_guard);
                    self.state.store(Status::Uninitialized, Ordering::Release);
                    return Err(error);
                }
            };

            // Write the initialized value to storage
            unsafe {
                // SAFETY:
                // - We have exclusive write access via CAS
                // - Pointer is derived from MaybeUninit
                let storage_ptr = (*self.storage.get()).as_mut_ptr();
                storage_ptr.write(initialized_value);
            };

            // Initialization succeeded, disarm panic guard
            core::mem::forget(panic_guard);

            // Mark as ready and make writes visible to other threads
            self.state.store(Status::Ready, Ordering::Release);

            // Return the initialized value
            return unsafe { Ok(self.get_value_unchecked()) };
        }
    }

    /// Blocks until the [`Once`] contains a value.
    ///
    /// Note that in releases prior to `0.7`, this function had the behaviour of [`Once::poll`].
    ///
    /// # Panics
    ///
    /// This function will panic if the [`Once`] previously failed while attempting
    /// to initialize. This is similar to the poisoning behaviour of `std::sync`'s
    /// primitives.
    pub fn wait(&self) -> &T {
        loop {
            if let Some(ready_value) = self.poll() {
                return ready_value;
            }
            // Yield to other threads while waiting
            core::hint::spin_loop();
        }
    }

    /// Like [`Once::get`], but will spin if the [`Once`] is in the process of being
    /// initialized. If initialization has not even begun, `None` will be returned.
    ///
    /// Note that in releases prior to `0.7`, this function was named `wait`.
    ///
    /// # Panics
    ///
    /// This function will panic if the [`Once`] previously failed while attempting
    /// to initialize. This is similar to the poisoning behaviour of `std::sync`'s
    /// primitives.
    pub fn poll(&self) -> Option<&T> {
        loop {
            // Check current initialization state
            let current_state = self.state.load(Ordering::Acquire);

            match current_state {
                Status::Uninitialized => return None,
                Status::Initializing => {
                    // Spin while another thread is initializing
                    core::hint::spin_loop();
                }
                Status::Ready => {
                    return Some(unsafe { self.get_value_unchecked() });
                }
                Status::Failed => {
                    panic!("OnceCell was contaminated by a previous panic")
                }
            }
        }
    }
}

impl<T> Once<T> {
    /// Initialization constant of [`Once`].
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self {
        state: AtomicStatus::new(Status::Uninitialized),
        storage: UnsafeCell::new(MaybeUninit::uninit()),
    };

    /// Creates a new [`Once`].
    pub const fn new() -> Self {
        Self::INIT
    }

    /// Creates a new initialized [`Once`].
    pub const fn initialized(data: T) -> Self {
        Self {
            state: AtomicStatus::new(Status::Ready),
            storage: UnsafeCell::new(MaybeUninit::new(data)),
        }
    }

    /// Retrieve a pointer to the inner data.
    ///
    /// While this method itself is safe, accessing the pointer before the [`Once`] has been
    /// initialized is UB, unless this method has already been written to from a pointer coming
    /// from this method.
    pub fn as_mut_ptr(&self) -> *mut T {
        // MaybeUninit<T> and T have identical memory layout
        self.storage.get().cast::<T>()
    }

    /// Get a reference to the initialized value. Must only be called when Ready.
    unsafe fn get_value_unchecked(&self) -> &T {
        // SAFETY:
        // - Caller ensures value is initialized
        // - Data is immutable after initialization
        unsafe { &*(*self.storage.get()).as_ptr() }
    }

    /// Get a mutable reference to the initialized value. Must only be called when Ready.
    unsafe fn get_value_mut_unchecked(&mut self) -> &mut T {
        // SAFETY:
        // - Caller ensures value is initialized
        // - We have exclusive mutable access
        unsafe { &mut *(*self.storage.get()).as_mut_ptr() }
    }

    /// Extract the initialized value. Must only be called when Ready.
    unsafe fn extract_value_unchecked(self) -> T {
        // SAFETY:
        // - Caller ensures value is initialized
        // - We own the Once, so we can move the value out
        unsafe { (*self.storage.get()).as_ptr().read() }
    }

    /// Returns a reference to the inner value if the [`Once`] has been initialized.
    pub fn get(&self) -> Option<&T> {
        // Check if initialization is complete
        let current_state = self.state.load(Ordering::Acquire);

        if current_state == Status::Ready {
            Some(unsafe { self.get_value_unchecked() })
        } else {
            None
        }
    }

    /// Returns a reference to the inner value on the unchecked assumption that the  [`Once`] has been initialized.
    ///
    /// # Safety
    ///
    /// This is *extremely* unsafe if the `Once` has not already been initialized because a reference to uninitialized
    /// memory will be returned, immediately triggering undefined behaviour (even if the reference goes unused).
    /// However, this can be useful in some instances for exposing the `Once` to FFI or when the overhead of atomically
    /// checking initialization is unacceptable and the `Once` has already been initialized.
    pub unsafe fn get_unchecked(&self) -> &T {
        debug_assert_eq!(
            self.state.load(Ordering::SeqCst),
            Status::Ready,
            "Attempted to access an uninitialized OnceCell. If this was run without debug checks, \
             this would be undefined behaviour. This is a serious bug and you must fix it.",
        );
        unsafe { self.get_value_unchecked() }
    }

    /// Returns a mutable reference to the inner value if the [`Once`] has been initialized.
    ///
    /// Because this method requires a mutable reference to the [`Once`], no synchronization
    /// overhead is required to access the inner value. In effect, it is zero-cost.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        if *self.state.get_mut() == Status::Ready {
            Some(unsafe { self.get_value_mut_unchecked() })
        } else {
            None
        }
    }

    /// Returns a mutable reference to the inner value
    ///
    /// # Safety
    ///
    /// This is *extremely* unsafe if the `Once` has not already been initialized because a reference to uninitialized
    /// memory will be returned, immediately triggering undefined behaviour (even if the reference goes unused).
    /// However, this can be useful in some instances for exposing the `Once` to FFI or when the overhead of atomically
    /// checking initialization is unacceptable and the `Once` has already been initialized.
    pub unsafe fn get_mut_unchecked(&mut self) -> &mut T {
        debug_assert_eq!(
            self.state.load(Ordering::SeqCst),
            Status::Ready,
            "Attempted to access an uninitialized OnceCell. If this was run without debug checks, \
             this would be undefined behavior. This is a serious bug and you must fix it.",
        );
        unsafe { self.get_value_mut_unchecked() }
    }

    /// Returns a the inner value if the [`Once`] has been initialized.
    ///
    /// Because this method requires ownership of the [`Once`], no synchronization overhead
    /// is required to access the inner value. In effect, it is zero-cost.
    pub fn try_into_inner(mut self) -> Option<T> {
        if *self.state.get_mut() == Status::Ready {
            Some(unsafe { self.extract_value_unchecked() })
        } else {
            None
        }
    }

    /// Returns a the inner value if the [`Once`] has been initialized.
    /// # Safety
    ///
    /// This is *extremely* unsafe if the `Once` has not already been initialized because a reference to uninitialized
    /// memory will be returned, immediately triggering undefined behaviour (even if the reference goes unused)
    /// This can be useful, if `Once` has already been initialized, and you want to bypass an
    /// option check.
    pub unsafe fn into_inner_unchecked(self) -> T {
        debug_assert_eq!(
            self.state.load(Ordering::SeqCst),
            Status::Ready,
            "Attempted to access an uninitialized OnceCell. If this was run without debug checks, \
             this would be undefined behavior. This is a serious bug and you must fix it.",
        );
        unsafe { self.extract_value_unchecked() }
    }

    /// Checks whether the value has been initialized.
    ///
    /// This is done using [`Acquire`](core::sync::atomic::Ordering::Acquire) ordering, and
    /// therefore it is safe to access the value directly via
    /// [`get_unchecked`](Self::get_unchecked) if this returns true.
    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == Status::Ready
    }
}

impl<T> From<T> for Once<T> {
    fn from(data: T) -> Self {
        Self::initialized(data)
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        // Exclusive mutable access means no atomic operations needed
        if *self.state.get_mut() == Status::Ready {
            unsafe {
                // Value is initialized, so we must drop it
                core::ptr::drop_in_place((*self.storage.get()).as_mut_ptr());
            }
        }
    }
}

struct PanicGuard<'a> {
    state: &'a AtomicStatus,
}

impl<'a> Drop for PanicGuard<'a> {
    fn drop(&mut self) {
        // Mark the cell as failed if we're unwinding from a panic
        // SeqCst ensures proper ordering even in presence of compiler bugs
        self.state.store(Status::Failed, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        prelude::v1::*,
        sync::{Arc, atomic::AtomicU32, mpsc::channel},
        thread,
    };

    use super::*;

    #[test]
    fn smoke_once() {
        static O: Once = Once::new();
        let mut a = 0;
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
    }

    #[test]
    fn smoke_once_value() {
        static O: Once<usize> = Once::new();
        let a = O.call_once(|| 1);
        assert_eq!(*a, 1);
        let b = O.call_once(|| 2);
        assert_eq!(*b, 1);
    }

    #[test]
    fn stampede_once() {
        static O: Once = Once::new();
        static mut RUN: bool = false;

        let (tx, rx) = channel();
        let mut ts = Vec::new();
        for _ in 0..10 {
            let tx = tx.clone();
            ts.push(thread::spawn(move || {
                for _ in 0..4 {
                    thread::yield_now()
                }
                unsafe {
                    O.call_once(|| {
                        assert!(!RUN);
                        RUN = true;
                    });
                    assert!(RUN);
                }
                tx.send(()).unwrap();
            }));
        }

        unsafe {
            O.call_once(|| {
                assert!(!RUN);
                RUN = true;
            });
            assert!(RUN);
        }

        for _ in 0..10 {
            rx.recv().unwrap();
        }

        for t in ts {
            t.join().unwrap();
        }
    }

    #[test]
    fn get() {
        static INIT: Once<usize> = Once::new();

        assert!(INIT.get().is_none());
        INIT.call_once(|| 2);
        assert_eq!(INIT.get().map(|r| *r), Some(2));
    }

    #[test]
    fn get_no_wait() {
        static INIT: Once<usize> = Once::new();

        assert!(INIT.get().is_none());
        let t = thread::spawn(move || {
            INIT.call_once(|| {
                thread::sleep(std::time::Duration::from_secs(3));
                42
            });
        });
        assert!(INIT.get().is_none());

        t.join().unwrap();
    }

    #[test]
    fn poll() {
        static INIT: Once<usize> = Once::new();

        assert!(INIT.poll().is_none());
        INIT.call_once(|| 3);
        assert_eq!(INIT.poll().map(|r| *r), Some(3));
    }

    #[test]
    fn wait() {
        static INIT: Once<usize> = Once::new();

        let t = std::thread::spawn(|| {
            assert_eq!(*INIT.wait(), 3);
            assert!(INIT.is_completed());
        });

        for _ in 0..4 {
            thread::yield_now()
        }

        assert!(INIT.poll().is_none());
        INIT.call_once(|| 3);

        t.join().unwrap();
    }

    #[test]
    fn panic() {
        use std::panic;

        static INIT: Once = Once::new();

        // poison the once
        let t = panic::catch_unwind(|| {
            INIT.call_once(|| panic!());
        });
        assert!(t.is_err());

        // poisoning propagates
        let t = panic::catch_unwind(|| {
            INIT.call_once(|| {});
        });
        assert!(t.is_err());
    }

    #[test]
    fn init_constant() {
        static O: Once = Once::INIT;
        let mut a = 0;
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
    }

    static mut CALLED: bool = false;

    struct DropTest {}

    impl Drop for DropTest {
        fn drop(&mut self) {
            unsafe {
                CALLED = true;
            }
        }
    }

    #[test]
    fn try_call_once_err() {
        let once = Once::<_>::new();
        let shared = Arc::new((once, AtomicU32::new(0)));

        let (tx, rx) = channel();

        let t0 = {
            let shared = shared.clone();
            thread::spawn(move || {
                let (once, called) = &*shared;

                once.try_call_once(|| {
                    called.fetch_add(1, Ordering::AcqRel);
                    tx.send(()).unwrap();
                    thread::sleep(std::time::Duration::from_millis(50));
                    Err(())
                })
                .ok();
            })
        };

        let t1 = {
            let shared = shared.clone();
            thread::spawn(move || {
                rx.recv().unwrap();
                let (once, called) = &*shared;
                assert_eq!(
                    called.load(Ordering::Acquire),
                    1,
                    "leader thread did not run first"
                );

                once.call_once(|| {
                    called.fetch_add(1, Ordering::AcqRel);
                });
            })
        };

        t0.join().unwrap();
        t1.join().unwrap();

        assert_eq!(shared.1.load(Ordering::Acquire), 2);
    }

    // This is sort of two test cases, but if we write them as separate test methods
    // they can be executed concurrently and then fail some small fraction of the
    // time.
    #[test]
    fn drop_occurs_and_skip_uninit_drop() {
        unsafe {
            CALLED = false;
        }

        {
            let once = Once::<_>::new();
            once.call_once(|| DropTest {});
        }

        assert!(unsafe { CALLED });
        // Now test that we skip drops for the uninitialized case.
        unsafe {
            CALLED = false;
        }

        let once = Once::<DropTest>::new();
        drop(once);

        assert!(unsafe { !CALLED });
    }

    #[test]
    fn call_once_test() {
        for _ in 0..20 {
            use std::{
                sync::{Arc, atomic::AtomicUsize},
                time::Duration,
            };
            let share = Arc::new(AtomicUsize::new(0));
            let once = Arc::new(Once::<_>::new());
            let mut hs = Vec::new();
            for _ in 0..8 {
                let h = thread::spawn({
                    let share = share.clone();
                    let once = once.clone();
                    move || {
                        thread::sleep(Duration::from_millis(10));
                        once.call_once(|| {
                            share.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                });
                hs.push(h);
            }
            for h in hs {
                h.join().unwrap();
            }
            assert_eq!(1, share.load(Ordering::SeqCst));
        }
    }
}
