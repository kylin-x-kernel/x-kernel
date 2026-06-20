// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(any(
    target_os = "linux",
    target_os = "redox",
    target_os = "dragonfly",
    target_os = "fuchsia"
))]
mod ffi {
    extern "C" {
        pub fn __errno_location() -> *mut i32;
    }

    pub fn errno() -> *mut i32 {
        // SAFETY: `__errno_location` is the C runtime accessor for the current
        // thread's `errno` slot on these targets. Calling it only obtains the
        // raw slot pointer; dereferencing that pointer remains the caller's
        // responsibility.
        unsafe { __errno_location() }
    }
}

#[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
mod ffi {
    extern "C" {
        pub fn __errno() -> *mut i32;
    }

    pub fn errno() -> *mut i32 {
        // SAFETY: `__errno` is the target C runtime accessor for the current
        // thread's `errno` slot. This wrapper only returns the raw pointer and
        // does not dereference it.
        unsafe { __errno() }
    }
}

#[cfg(any(target_os = "freebsd", target_os = "ios", target_os = "macos"))]
mod ffi {
    extern "C" {
        pub fn __error() -> *mut i32;
    }

    pub fn errno() -> *mut i32 {
        // SAFETY: `__error` is the target C runtime accessor for the current
        // thread's `errno` slot. This wrapper only returns the raw pointer and
        // does not dereference it.
        unsafe { __error() }
    }
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
mod ffi {
    extern "C" {
        pub fn ___errno() -> *mut i32;
    }

    pub fn errno() -> *mut i32 {
        // SAFETY: `___errno` is the target C runtime accessor for the current
        // thread's `errno` slot. This wrapper only returns the raw pointer and
        // does not dereference it.
        unsafe { ___errno() }
    }
}

pub use ffi::errno;
