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

/// Returns whether the current CPU is executing inside an exception context.
#[inline]
pub fn in_exception_context() -> bool {
    // SAFETY: `current_ref_raw()` returns a raw pointer to this CPU's per-CPU
    // data area. The pointer is written only by this CPU's exception guard while
    // IRQs are off, so Relaxed ordering is sufficient for this local state.
    unsafe {
        ACTIVE_EXCEPTION_CONTEXT_PTR
            .current_ref_raw()
            .load(Ordering::Relaxed)
            != 0
    }
}

/// Returns a copy of the currently active trapframe, if any.
///
/// The active trapframe itself lives on the current CPU's trap stack. This
/// API returns a by-value snapshot so callers cannot accidentally retain a
/// borrowed reference after the trap handler unwinds.
#[inline]
pub fn active_exception_context() -> Option<ExceptionContext> {
    // SAFETY: `current_ref_raw()` returns a raw pointer to this CPU's per-CPU
    // data area. The `load` is a best-effort atomic read; Relaxed ordering is
    // sufficient because each CPU has its own copy and only the current CPU
    // writes to it (within a trap handler, with IRQs off).
    let ptr = unsafe {
        ACTIVE_EXCEPTION_CONTEXT_PTR
            .current_ref_raw()
            .load(Ordering::Relaxed)
    };

    if ptr == 0 {
        None
    } else {
        // SAFETY: `ptr` was installed from a live `&ExceptionContext` by
        // `ExceptionContextGuard::new` on this CPU. We copy the trapframe by
        // value immediately instead of returning a borrowed reference, so the
        // result does not outlive the trap-stack storage it came from.
        Some(unsafe { *(ptr as *const ExceptionContext) })
    }
}

/// Calls `f` with a reference to a snapshot of the currently active trapframe.
///
/// Unlike the trapframe pointer tracked by `ExceptionContextGuard`, the
/// reference passed to `f` points to a temporary local snapshot created by
/// `active_exception_context()`. Callers can inspect register contents inside
/// the closure, but must not assume pointer identity with the live trap-stack
/// frame.
#[inline]
pub fn with_active_exception_context<T>(f: impl FnOnce(Option<&ExceptionContext>) -> T) -> T {
    let snapshot = active_exception_context();
    f(snapshot.as_ref())
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

        // SAFETY: `current_ref_raw()` points to this CPU's per-CPU data.
        // The swap is safe because: (1) per-CPU data is uniquely owned by
        // this CPU; (2) trap handlers run with IRQs off, so no concurrent
        // access from the same CPU; (3) `ptr` is a valid address derived
        // from a live `&ExceptionContext` reference.
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
        // SAFETY: Restoring the previous pointer is safe because `self.prev`
        // was obtained from the same per-CPU atomic in `new()`. Drop runs
        // at the end of the trap handler scope, still with IRQs off.
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

    #[def_test(serial)]
    fn test_active_exception_context_none() {
        assert!(active_exception_context().is_none());
        assert!(!in_exception_context());
    }

    #[def_test(serial)]
    fn test_guard_sets_and_restores() {
        let ctx = ExceptionContext::default();
        {
            let _guard = ExceptionContextGuard::new(&ctx);
            assert!(active_exception_context().is_some());
            assert!(in_exception_context());
        }
        assert!(active_exception_context().is_none());
        assert!(!in_exception_context());
    }

    #[def_test(serial)]
    fn test_with_active_exception_context() {
        let mut ctx = ExceptionContext::default();
        ctx.set_retval(0x1234);
        ctx.set_ip(0x5678);
        ctx.set_arg1(0x9abc);
        let _guard = ExceptionContextGuard::new(&ctx);
        assert!(in_exception_context());
        let got = with_active_exception_context(|opt| opt.copied());
        let got = got.expect("active exception context snapshot missing");
        assert_eq!(got.retval(), ctx.retval());
        assert_eq!(got.ip(), ctx.ip());
        assert_eq!(got.arg1(), ctx.arg1());
    }

    #[def_test(serial)]
    fn test_nested_exception_context_state() {
        let outer = ExceptionContext::default();
        let inner = ExceptionContext::default();

        {
            let _outer_guard = ExceptionContextGuard::new(&outer);
            assert!(in_exception_context());
            {
                let _inner_guard = ExceptionContextGuard::new(&inner);
                assert!(in_exception_context());
            }
            assert!(in_exception_context());
        }

        assert!(!in_exception_context());
    }
}
