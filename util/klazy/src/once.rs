// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronization primitives for one-time evaluation.
//!
//! See [`Once`] for the main type.

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
/// This type is `no_std`-compatible and uses spin-wait for coordination during
/// initialization. After initialization, access is a single atomic load.
///
/// # Poisoning
///
/// If the initialization closure panics, the `Once` is permanently poisoned. All subsequent
/// calls to [`call_once`](Self::call_once), [`try_call_once`](Self::try_call_once),
/// [`wait`](Self::wait), or [`poll`](Self::poll) will panic.
///
/// For fallible initialization, use [`try_call_once`](Self::try_call_once) instead — if the
/// closure returns `Err`, the cell is reset to `Uninitialized` and can be retried, rather
/// than being permanently poisoned.
///
/// # Examples
///
/// ```
/// use klazy;
///
/// static START: klazy::Once = klazy::Once::new();
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
// SAFETY: After initialization, `Once<T>` only exposes shared references to
// `T`, and initialization is serialized by the state machine, so sharing across
// threads is sound when `T: Send + Sync`.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
// SAFETY: Moving `Once<T>` between threads is sound because the only shared
// mutable state is guarded by the atomic initialization protocol, and `T` may
// be transferred across threads when `T: Send`.
unsafe impl<T: Send> Send for Once<T> {}

mod status {
    use super::*;

    // SAFETY: This structure has an invariant, namely that the inner atomic u8 must *always* have
    // a value for which there exists a valid Status. This means that users of this API must only
    // be allowed to load and store `Status`es.
    #[repr(transparent)]
    pub(super) struct AtomicStatus(AtomicU8);

    /// Four possible states for the `Once` cell, encoded as `u8` discriminants in
    /// the atomic state word. Represents the lifecycle:
    /// `Uninitialized` → `Initializing` → `Ready` / `Failed`
    #[repr(u8)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) enum Status {
        Uninitialized = 0x00,
        Initializing  = 0x01,
        Ready         = 0x02,
        Failed        = 0x03,
    }
    impl Status {
        fn from_raw(raw: u8) -> Self {
            match raw {
                0x00 => Self::Uninitialized,
                0x01 => Self::Initializing,
                0x02 => Self::Ready,
                0x03 => Self::Failed,
                _ => unreachable!("invalid Once status: {raw:#x}"),
            }
        }
    }

    impl AtomicStatus {
        #[inline(always)]
        pub(super) const fn new(status: Status) -> Self {
            // Convert Status enum to underlying u8 representation
            Self(AtomicU8::new(status as u8))
        }

        #[inline(always)]
        pub(super) fn load(&self, ordering: Ordering) -> Status {
            // The invariant ensures the loaded u8 is a valid Status variant
            let raw_value = self.0.load(ordering);
            Status::from_raw(raw_value)
        }

        #[inline(always)]
        pub(super) fn store(&self, status: Status, ordering: Ordering) {
            // Store the enum discriminant value directly
            let raw_value = status as u8;
            self.0.store(raw_value, ordering);
        }

        #[inline(always)]
        pub(super) fn compare_exchange(
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
                Ok(current) => Ok(Status::from_raw(current)),
                Err(actual) => Err(Status::from_raw(actual)),
            }
        }

        #[inline(always)]
        pub(super) fn get_mut(&mut self) -> Status {
            Status::from_raw(*self.0.get_mut())
        }
    }
}
use self::status::{AtomicStatus, Status};

impl<T> Once<T> {
    /// Performs an initialization routine once and only once. The given closure
    /// will be executed if this is the first time `call_once` has been called,
    /// and otherwise the routine will *not* be invoked.
    ///
    /// This method will spin-wait if another thread is currently performing
    /// initialization.
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
    /// use klazy;
    ///
    /// static INIT: klazy::Once<usize> = klazy::Once::new();
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
    /// fail, and leaves the `Once` in an uninitialized state if it does.
    ///
    /// This method will spin-wait if another thread is currently performing
    /// initialization.
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
    /// use klazy;
    ///
    /// static INIT: klazy::Once<usize> = klazy::Once::new();
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
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: A previous initializer \
                              panicked and left no value; like std's poisoned Once and LazyLock, \
                              the value can never be produced"
                )]
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
                    // SAFETY: `Status::Ready` is only published after the value
                    // has been fully initialized in `storage`.
                    return Ok(unsafe { self.get_value_unchecked() });
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

            // SAFETY: The successful CAS gives this thread exclusive initialization
            // rights. MaybeUninit<T> has T's layout, and storage belongs to this cell.
            unsafe { self.storage.get().cast::<T>().write(initialized_value) };

            // Initialization succeeded, disarm panic guard
            core::mem::forget(panic_guard);

            // Mark as ready and make writes visible to other threads
            self.state.store(Status::Ready, Ordering::Release);

            // Return the initialized value
            // SAFETY: This thread just wrote the value to `storage` and then
            // published `Status::Ready`, so the value is initialized here.
            return unsafe { Ok(self.get_value_unchecked()) };
        }
    }

    /// Blocks until the [`Once`] contains a value.
    ///
    /// This method **spin-waits** using `core::hint::spin_loop` while another thread
    /// is initializing. There is no timeout or blocking primitive involved. Use this
    /// only when initialization is expected to complete quickly (e.g., kernel early boot).
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

    /// Like [`Once::get`], but will **spin-wait** if the [`Once`] is in the process
    /// of being initialized. If initialization has not even begun, `None` will be
    /// returned.
    ///
    /// This method uses `core::hint::spin_loop` to wait. There is no timeout.
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
                    // SAFETY: `Status::Ready` is only observable after the
                    // initialized value has been published to `storage`.
                    return Some(unsafe { self.get_value_unchecked() });
                }
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: A previous initializer \
                              panicked and left no value; like std's poisoned Once and LazyLock, \
                              the value can never be produced"
                )]
                Status::Failed => {
                    panic!("OnceCell was contaminated by a previous panic")
                }
            }
        }
    }
}

impl<T> Once<T> {
    /// A const-initialized [`Once`] in the `Uninitialized` state.
    ///
    /// Equivalent to [`Once::new()`], but as an associated constant.
    #[expect(
        clippy::declare_interior_mutable_const,
        reason = "CG-024-lint-exceptions: clippy::declare_interior_mutable_const: INIT constructs \
                  independent Once cells; it does not name shared mutable storage"
    )]
    pub const INIT: Self = Self {
        state: AtomicStatus::new(Status::Uninitialized),
        storage: UnsafeCell::new(MaybeUninit::uninit()),
    };

    /// Creates a new [`Once`] in the `Uninitialized` state.
    ///
    /// The returned cell does not contain a value and must be initialized via
    /// [`call_once`](Self::call_once) or [`try_call_once`](Self::try_call_once).
    pub const fn new() -> Self {
        Self::INIT
    }

    /// Creates a new [`Once`] in the `Ready` state with the given value.
    ///
    /// The cell behaves as if initialization has already completed. Subsequent
    /// calls to [`get`](Self::get) will return `Some(&data)`.
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
    ///
    /// # Safety
    ///
    /// The value must be initialized and published as `Status::Ready`. The
    /// caller must have initialized it, observed publication with acquire
    /// ordering, or own the cell exclusively. Keep it initialized for the
    /// borrow and permit mutation only through its interior-mutable fields.
    unsafe fn get_value_unchecked(&self) -> &T {
        // SAFETY: The caller observed Ready after initialization, and the cell
        // keeps T live without further mutation. MaybeUninit<T> shares T's layout.
        unsafe { &*self.storage.get().cast::<T>() }
    }

    /// Get a mutable reference to the initialized value. Must only be called when Ready.
    ///
    /// # Safety
    ///
    /// The cell must be `Status::Ready` with a fully initialized value that
    /// has not been moved out. The exclusive borrow covers all value access.
    unsafe fn get_value_mut_unchecked(&mut self) -> &mut T {
        // SAFETY:
        // - Caller ensures value is initialized
        // - We have exclusive mutable access
        unsafe { self.storage.get_mut().assume_init_mut() }
    }

    /// Moves the initialized value out of storage.
    ///
    /// # Safety
    ///
    /// The cell must contain a fully initialized value that has not been moved
    /// out. Consuming the cell gives this operation exclusive access to storage;
    /// the operation suppresses the cell destructor when moving the value out.
    unsafe fn extract_value_unchecked(self) -> T {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY:
        // - Caller ensures value is initialized
        // - We own the Once, so we can move the value out
        // - ManuallyDrop prevents the Ready cell from dropping the moved value
        // - MaybeUninit<T> shares T's layout, so the cast preserves alignment
        unsafe { this.storage.get().cast::<T>().read() }
    }

    /// Returns a reference to the inner value if the [`Once`] has been initialized.
    ///
    /// This is a non-blocking, non-panicking check using a single `Acquire` load.
    /// Returns `None` if initialization has not completed, is in progress, or has
    /// failed (poisoned).
    pub fn get(&self) -> Option<&T> {
        // Check if initialization is complete
        let current_state = self.state.load(Ordering::Acquire);

        if current_state == Status::Ready {
            // SAFETY: `Status::Ready` is only observable after the initialized
            // value has been published to `storage`.
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
        // SAFETY: The caller promises the cell is initialized; the debug
        // assertion checks that contract in debug builds.
        unsafe { self.get_value_unchecked() }
    }

    /// Returns a mutable reference to the inner value if the [`Once`] has been initialized.
    ///
    /// Because this method requires a mutable reference to the [`Once`], no synchronization
    /// overhead is required to access the inner value. In effect, it is zero-cost.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.state.get_mut() == Status::Ready {
            // SAFETY: `Status::Ready` guarantees initialization and `&mut self`
            // guarantees exclusive access to the stored value.
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
        // SAFETY: The caller promises the cell is initialized; the debug
        // assertion checks that contract in debug builds.
        unsafe { self.get_value_mut_unchecked() }
    }

    /// Returns the inner value if the [`Once`] has been initialized.
    ///
    /// Because this method requires ownership of the [`Once`], no synchronization overhead
    /// is required to access the inner value. In effect, it is zero-cost. Returns `None`
    /// if the cell is not in the `Ready` state.
    pub fn try_into_inner(mut self) -> Option<T> {
        if self.state.get_mut() == Status::Ready {
            // SAFETY: `Status::Ready` guarantees initialization and ownership
            // of `self` lets us move the value out exactly once.
            Some(unsafe { self.extract_value_unchecked() })
        } else {
            None
        }
    }

    /// Returns the inner value if the [`Once`] has been initialized.
    ///
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
        // SAFETY: The caller promises the cell is initialized; the debug
        // assertion checks that contract in debug builds.
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
        if self.state.get_mut() == Status::Ready {
            // SAFETY: `Status::Ready` guarantees the slot contains a live `T`,
            // and `&mut self` guarantees exclusive access during drop.
            unsafe {
                self.storage.get_mut().assume_init_drop();
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

// Gate before resolving the macro: boot loaders use this crate without an allocator.
#[cfg(unittest)]
unittest::path_mod_test!(unittest_tests, "../ut-tests/once_unittest.rs");

#[cfg(test)]
#[path = "../tests/host/once_host.rs"]
mod once_host_tests;
