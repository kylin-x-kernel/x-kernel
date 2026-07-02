// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lazily initialized statics backed by [`crate::Lazy`].
//!
//! Each `static ref` expands to a distinct, unnameable wrapper type that
//! [`Deref`]s to the inner value `T`, mirroring the upstream `lazy_static`
//! crate. Because the wrapper type intentionally does not implement
//! `Clone`/`Copy`, trait-method lookup on a `static NAME: ...` falls through
//! the `Deref` impl to the inner value `T`. That is what makes idioms like
//! `STATIC.clone()` (when `T: Clone`, e.g. `Arc<_>`) or `STATIC.lock()`
//! behave exactly as if the value were stored inline — matching upstream
//! `lazy_static` dispatch semantics.
//!
//! Initialization is protected by the internal [`crate::Once`] spin-wait.
//!
//! # Examples
//!
//! ```rust,ignore
//! use klazy::lazy_static;
//!
//! lazy_static! {
//!     static ref TABLE: spin::Mutex<HashMap<u32, String>> =
//!         spin::Mutex::new(HashMap::new());
//! }
//!
//! fn lookup(key: u32) -> Option<String> {
//!     TABLE.lock().get(&key).cloned()
//! }
//! ```
//!
//! # Trait method dispatch
//!
//! Because the generated type derefs to `T`, methods on `T` (and anything `T`
//! itself derefs to) resolve automatically:
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use klazy::lazy_static;
//!
//! struct Driver;
//! impl Driver { fn ping(&self) -> bool { true } }
//!
//! lazy_static! {
//!     static ref DRV: Arc<Driver> = Arc::new(Driver);
//! }
//!
//! // `DRV.clone()` returns `Arc<Driver>`, and `DRV.ping()` reaches `Driver::ping`.
//! let _another: Arc<Driver> = DRV.clone();
//! assert!(DRV.ping());
//! ```

// `Deref` and `Lazy` are referenced directly inside the `paste::item!`
// expansion below, so they must be in scope at the macro definition site.
#[allow(unused_imports)]
use core::ops::Deref;

#[allow(unused_imports)]
use crate::Lazy;

/// Declares lazily-initialized static variables.
///
/// Each `static ref NAME: T = init;` produces a `static NAME: <wrapper>`
/// whose wrapper type derefs to `T`, so methods and trait impls of `T` (and
/// anything `T` itself derefs to) resolve transparently through the static.
/// This matches the dispatch semantics of the upstream `lazy_static` crate:
/// `NAME.clone()` invokes `T::clone` (via autoderef), not a clone of the
/// wrapper.
///
/// See the module documentation for examples.
#[macro_export]
macro_rules! lazy_static {
    ($(#[$attr:meta])* $vis:vis static ref $name:ident : $ty:ty = $init:expr; $($rest:tt)*) => {
        $crate::__lazy_static_inner!($(#[$attr])* $vis $name, $ty, $init);
        $crate::lazy_static!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static ref $name:ident : $ty:ty = $init:expr) => {
        $crate::__lazy_static_inner!($(#[$attr])* $vis $name, $ty, $init);
    };
    () => {};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __lazy_static_inner {
    ($(#[$attr:meta])* $vis:vis $name:ident, $ty:ty, $init:expr) => {
        $crate::__paste::item! {
            // Private initializer function. The name is derived from `$name`
            // so multiple `lazy_static!` items can coexist in the same scope
            // without colliding. The `non_snake_case` allow is because the
            // spliced ident (e.g. `N_TTY__init`) carries the user's UPPER
            // static name as a prefix.
            #[doc(hidden)]
            #[allow(non_snake_case)]
            fn [<$name __init>]() -> $ty { $init }

            // Backing storage, lazily initialized via `Lazy` and never exposed
            // directly. The explicit `fn() -> $ty` factory type keeps it
            // consistent regardless of the inference of the fn-item type.
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static [<$name __LAZY>]: $crate::Lazy<$ty, fn() -> $ty> =
                $crate::Lazy::new([<$name __init>] as fn() -> $ty);

            // Per-static wrapper type. It is zero-sized (a unit struct),
            // unnameable by convention, and intentionally **does not**
            // implement `Clone`/`Copy`: that is what makes `NAME.clone()`
            // resolve to `T::clone` through the `Deref` impl below instead of
            // cloning the wrapper. Each static gets its own distinct wrapper
            // type, so two statics are never interchangeable — matching the
            // upstream `lazy_static` isolation model.
            //
            // The wrapper visibility tracks `$vis` so a `pub static` can be
            // referenced from other crates (the static's type must be at
            // least as visible as the static itself); `#[doc(hidden)]` keeps
            // it out of the rendered API surface.
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            $vis struct [<$name __LazyRef>];

            impl ::core::ops::Deref for [<$name __LazyRef>] {
                type Target = $ty;
                fn deref(&self) -> &$ty {
                    // `Lazy<T>: Deref<Target = T>`; `Lazy::force` performs the
                    // one-time initialization and returns `&T`.
                    $crate::Lazy::force(&[<$name __LAZY>])
                }
            }

            // The user-visible static. Its type derefs to `$ty`, so
            // `NAME.clone()` resolves to `<$ty>::clone` via autoderef instead
            // of cloning the wrapper — matching upstream `lazy_static`
            // semantics.
            $(#[$attr])*
            $vis static $name: [<$name __LazyRef>] = [<$name __LazyRef>];
        }
    };
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::lazy_static;

    struct Counter;

    impl Counter {
        fn ping(&self) -> bool {
            true
        }
    }

    lazy_static! {
        /// A lazily initialized `Arc<Counter>`.
        static ref DRV: Arc<Counter> = Arc::new(Counter);
        static ref VALUE: u32 = 42;
    }

    /// `STATIC.clone()` must resolve to `T::clone` via autoderef (the upstream
    /// `lazy_static` dispatch contract), not clone the wrapper.
    #[test]
    fn clone_returns_inner_type() {
        let cloned: Arc<Counter> = DRV.clone();
        assert!(Arc::ptr_eq(&cloned, &*DRV));
    }

    /// Methods on the inner type resolve through `Deref`.
    #[test]
    fn inner_methods_dispatch() {
        assert!(DRV.ping());
    }

    /// `Deref` reaches the underlying value and triggers one-time init.
    #[test]
    fn deref_initializes_and_reads() {
        assert_eq!(*VALUE, 42);
    }

    /// A `static ref` may be declared `pub`; the inner value is reachable
    /// from sibling code paths that only see the static.
    #[test]
    fn pub_static_is_usable() {
        let v: &u32 = &*VALUE;
        assert_eq!(*v, 42);
    }
}
