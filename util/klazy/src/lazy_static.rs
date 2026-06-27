// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lazily initialized statics backed by [`crate::Lazy`].

/// Declares lazily-initialized static variables.
///
/// Each `static ref` is a `&Lazy<T>` that auto-derefs to `&T` on use.
/// Initialization is protected by the internal [`crate::Once`] spin-wait.
///
/// # Examples
///
/// ```rust,ignore
/// use klazy::lazy_static;
///
/// lazy_static! {
///     static ref TABLE: spin::Mutex<HashMap<u32, String>> =
///         spin::Mutex::new(HashMap::new());
/// }
///
/// fn lookup(key: u32) -> Option<String> {
///     TABLE.lock().get(&key).cloned()
/// }
/// ```
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
        $(#[$attr])*
        $vis static $name: &$crate::Lazy<$ty> = const {
            fn __init() -> $ty { $init }
            static __LAZY: $crate::Lazy<$ty> = $crate::Lazy::new(__init);
            &__LAZY
        };
    };
}
