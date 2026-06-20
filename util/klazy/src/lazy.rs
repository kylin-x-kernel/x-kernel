// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronization primitives for lazy evaluation.
//!
//! Implementation adapted from the `SyncLazy` type of the standard library. See:
//! <https://doc.rust-lang.org/std/lazy/struct.SyncLazy.html>
//!
//! See [`Lazy`] for the main type.

use core::{cell::Cell, fmt, ops::Deref};

use crate::once::Once;

/// A value which is initialized on the first access.
///
/// This type is a thread-safe `Lazy`, and can be used in statics. It is
/// `no_std`-compatible and uses spin-wait for coordination during initialization.
///
/// # Thread Safety
///
/// `Lazy<T, F>` implements `Sync` when `Once<T>: Sync`, which requires `T: Send + Sync`.
/// The factory function `F` does not need to be `Sync` — it is consumed via
/// `Cell::take()` under the `Once` synchronization, so only one thread ever reads it.
///
/// # Poisoning
///
/// If the initialization closure panics, the `Lazy` is permanently poisoned.
/// All subsequent accesses via [`Deref`] will panic. This matches the behavior
/// of `std::sync::SyncLazy`.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
///
/// use klazy::Lazy;
///
/// static HASHMAP: Lazy<HashMap<i32, String>> = Lazy::new(|| {
///     println!("initializing");
///     let mut m = HashMap::new();
///     m.insert(13, "Spica".to_string());
///     m.insert(74, "Hoyten".to_string());
///     m
/// });
///
/// fn main() {
///     println!("ready");
///     std::thread::spawn(|| {
///         println!("{:?}", HASHMAP.get(&13));
///     })
///     .join()
///     .unwrap();
///     println!("{:?}", HASHMAP.get(&74));
///
///     // Prints:
///     //   ready
///     //   initializing
///     //   Some("Spica")
///     //   Some("Hoyten")
/// }
/// ```
pub struct Lazy<T, F = fn() -> T> {
    /// Underlying once-initialized storage container
    value_storage: Once<T>,
    /// Factory function holder with interior mutability pattern
    factory: Cell<Option<F>>,
}

impl<T: fmt::Debug, F> fmt::Debug for Lazy<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Build debug representation as tuple structure
        let mut debug_builder = f.debug_tuple("LazyValue");

        // Display content based on initialization state
        let debug_builder = match self.value_storage.get() {
            Some(content) => debug_builder.field(&content),
            None => debug_builder.field(&format_args!("<uninit>")),
        };

        debug_builder.finish()
    }
}

// We never create a `&F` from a `&Lazy<T, F>` so it is fine
// to not impl `Sync` for `F`
// we do create a `&mut Option<F>` in `force`, but this is
// properly synchronized, so it only happens once
// so it also does not contribute to this impl.
// SAFETY: `Lazy` only shares access to the initialized `T`; access to the
// stored initializer is internally synchronized so `&Lazy<T, F>` never creates
// unsynchronized shared access to `F`.
unsafe impl<T, F: Send> Sync for Lazy<T, F> where Once<T>: Sync {}
// auto-derived `Send` impl is OK.

impl<T, F> Lazy<T, F> {
    /// Creates a new lazy value with the given initializing function.
    ///
    /// This is a `const` constructor and can be used to initialize statics.
    pub const fn new(f: F) -> Self {
        Self {
            value_storage: Once::new(),
            factory: Cell::new(Some(f)),
        }
    }

    /// Retrieves a mutable pointer to the inner data.
    ///
    /// This is especially useful when interfacing with low level code or FFI where the caller
    /// explicitly knows that it has exclusive access to the inner data. Note that reading from
    /// this pointer is UB until initialized or directly written to.
    pub fn as_mut_ptr(&self) -> *mut T {
        self.value_storage.as_mut_ptr()
    }
}

impl<T, F: FnOnce() -> T> Lazy<T, F> {
    /// Forces the evaluation of this lazy value and
    /// returns a reference to result. This is equivalent
    /// to the `Deref` impl, but is explicit.
    ///
    /// # Examples
    ///
    /// ```
    /// use klazy::Lazy;
    ///
    /// let lazy = Lazy::new(|| 92);
    ///
    /// assert_eq!(Lazy::force(&lazy), &92);
    /// assert_eq!(&*lazy, &92);
    /// ```
    pub fn force(this: &Self) -> &T {
        // Ensure single initialization through once-cell mechanism
        this.value_storage.call_once(|| {
            // Retrieve and invoke the factory function
            let factory_fn = this.factory.take();
            if let Some(creator) = factory_fn {
                creator()
            } else {
                panic!("LazyValue has been contaminated by previous panic")
            }
        })
    }
}

impl<T, F: FnOnce() -> T> Deref for Lazy<T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        Self::force(self)
    }
}

impl<T: Default> Default for Lazy<T, fn() -> T> {
    /// Creates a new lazy value using `Default` as the initializing function.
    fn default() -> Self {
        Self::new(T::default)
    }
}
