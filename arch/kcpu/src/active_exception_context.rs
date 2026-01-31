// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Active trapframe tracking.
//!
//! This module provides a tiny facility to expose the *current* trapframe
//! (register snapshot) to external callers (e.g. a pseudo-NMI watchdog).
//!
//! ## Design notes
//! - We only keep a single pointer per CPU *logically* (the most inner trap).
//! - The storage itself is a single atomic pointer. This is already useful on
//!   uniprocessor builds and is safe to call from interrupt/NMI-like contexts.
//! - If you need full per-CPU + nested trap support, this can be extended to a
//!   per-CPU stack later.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::ExceptionContext;

/// Stores the pointer to the currently active trapframe.
///
/// 0 means no active trapframe.
#[percpu::def_percpu]
static ACTIVE_EXCEPTION_CONTEXT_PTR: AtomicUsize = AtomicUsize::new(0);

/// Returns the currently active trapframe, if any.
///
/// # Safety & lifetime
/// The returned reference is valid only while the CPU is still in the trap
/// context where the trapframe lives on the stack. Therefore, callers should
/// treat it as a short-lived snapshot: use it immediately and don't store it.
#[inline]
pub fn active_exception_context() -> Option<&'static ExceptionContext> {
    // Safety: caller context must tolerate best-effort snapshot.
    let ptr = unsafe {
        ACTIVE_EXCEPTION_CONTEXT_PTR
            .current_ref_raw()
            .load(Ordering::Relaxed)
    };

    if ptr == 0 {
        None
    } else {
        // SAFETY:
        // - pointer was installed from a valid &ExceptionContext
        // - valid only while still in the trap context
        Some(unsafe { &*(ptr as *const ExceptionContext) })
    }
}

/// Calls `f` with the currently active trapframe.
#[inline]
pub fn with_active_exception_context<T>(f: impl FnOnce(Option<&ExceptionContext>) -> T) -> T {
    f(active_exception_context().map(|tf| tf as &ExceptionContext))
}

/// A guard that exposes `tf` as the active trapframe within a scope.
///
/// This is intended to be used at the beginning of a trap handler function:
///
/// ```no_run
/// fn trap_handler(tf: &mut ExceptionContext) {
///     let _guard = ExceptionContextGuard::new(tf);
///     // ...
/// }
/// ```
pub struct ExceptionContextGuard {
    prev: usize,
}

impl ExceptionContextGuard {
    /// Sets `tf` as the active trapframe and returns a guard which will restore
    /// the previous value on drop.
    #[inline]
    pub fn new(tf: &ExceptionContext) -> Self {
        let ptr = tf as *const ExceptionContext as usize;

        let prev = unsafe {
            ACTIVE_EXCEPTION_CONTEXT_PTR
                .current_ref_raw()
                .swap(ptr, Ordering::Relaxed)
        };

        Self { prev }
    }
}

impl Drop for ExceptionContextGuard {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ACTIVE_EXCEPTION_CONTEXT_PTR
                .current_ref_raw()
                .store(self.prev, Ordering::Relaxed);
        }
    }
}

#[cfg(unittest)]
pub mod tests_active_exception_context {
    use unittest::def_test;

    use super::*;
    use crate::ExceptionContext;

    #[def_test]
    fn test_active_exception_context_none() {
        assert!(active_exception_context().is_none());
    }

    #[def_test]
    fn test_guard_sets_and_restores() {
        let ctx = ExceptionContext::default();
        {
            let _guard = ExceptionContextGuard::new(&ctx);
            assert!(active_exception_context().is_some());
        }
        assert!(active_exception_context().is_none());
    }

    #[def_test]
    fn test_with_active_exception_context() {
        let ctx = ExceptionContext::default();
        let _guard = ExceptionContextGuard::new(&ctx);
        let got = with_active_exception_context(|opt| opt.map(|p| p as *const _));
        assert_eq!(got, Some(&ctx as *const _));
    }
}
