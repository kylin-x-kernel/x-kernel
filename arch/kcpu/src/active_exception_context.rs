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
//! - The scheduler clears or suspends the current CPU pointer before switching
//!   away from a task that installed it, and restores a suspended pointer when
//!   that task resumes, so sleepable trap backends do not leave stale per-CPU
//!   exception markers behind.
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
    // data area. The pointer is normally written by this CPU's exception guard
    // while IRQs are off. A delayed guard drop may restore its original slot
    // after the task migrated, but that path uses CAS and does not publish data
    // to other CPUs, so Relaxed ordering is sufficient for this diagnostic state.
    unsafe {
        ACTIVE_EXCEPTION_CONTEXT_PTR
            .current_ref_raw()
            .load(Ordering::Relaxed)
            != 0
    }
}

/// Returns a copy of the currently active trapframe, if any.
///
/// The active trapframe is owned by the currently executing trap handler stack
/// or task user context. This API returns a by-value snapshot so callers cannot
/// accidentally retain a borrowed reference after the trap handler unwinds.
#[inline]
pub fn active_exception_context() -> Option<ExceptionContext> {
    // SAFETY: `current_ref_raw()` returns a raw pointer to this CPU's per-CPU
    // data area. The `load` is a best-effort atomic read; Relaxed ordering is
    // sufficient because callers only need a best-effort diagnostic snapshot.
    // Delayed restoration after migration is guarded by compare_exchange.
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

/// Opaque move-only token for an exception context suspended across a task switch.
pub struct SuspendedExceptionContext {
    ptr: usize,
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
    slot: *const AtomicUsize,
    ptr: usize,
    prev: usize,
}

impl ExceptionContextGuard {
    /// Sets `tf` as the active trapframe and returns a guard which will restore
    /// the previous value on drop.
    #[inline]
    pub fn new(tf: &ExceptionContext) -> Self {
        let ptr = tf as *const ExceptionContext as usize;

        // SAFETY: `current_ref_raw()` points to this CPU's per-CPU data.
        // The swap is safe because: (1) the slot address is captured before
        // any trap backend can block or migrate; (2) trap handlers run with
        // IRQs off on entry, so no concurrent same-CPU trap install races this
        // operation; (3) `ptr` is derived from a live `&ExceptionContext`.
        let slot = unsafe { ACTIVE_EXCEPTION_CONTEXT_PTR.current_ref_raw() };
        let prev = slot.swap(ptr, Ordering::Relaxed);

        Self { slot, ptr, prev }
    }
}

/// Clears this CPU's active exception context and returns a token that can
/// restore the same context when this task is scheduled again.
#[inline]
pub fn suspend_active_exception_context() -> SuspendedExceptionContext {
    // SAFETY: `current_ref_raw()` points to this CPU's per-CPU data. Scheduling
    // owns the current CPU here and is about to switch away from the task whose
    // trap stack owns the pointer.
    let slot = unsafe { ACTIVE_EXCEPTION_CONTEXT_PTR.current_ref_raw() };
    SuspendedExceptionContext {
        ptr: slot.swap(0, Ordering::Relaxed),
    }
}

/// Restores a context previously suspended by `suspend_active_exception_context`
/// after the owning task is scheduled back on the current CPU.
#[inline]
pub fn resume_active_exception_context(suspended: SuspendedExceptionContext) {
    if suspended.ptr == 0 {
        return;
    }

    // SAFETY: `current_ref_raw()` points to this CPU's per-CPU data. The token
    // is restored only after the owning task's saved context has resumed on this
    // CPU, so the pointer again describes this CPU's active trap stack.
    let slot = unsafe { ACTIVE_EXCEPTION_CONTEXT_PTR.current_ref_raw() };
    slot.store(suspended.ptr, Ordering::Relaxed);
}

impl ExceptionContextGuard {
    #[inline]
    fn restore_slot(&self) {
        // SAFETY: `self.slot` is the exact per-CPU atomic updated in `new()`.
        // Per-CPU storage is allocated for the whole kernel lifetime, so the
        // pointer remains valid even if this task migrated before `Drop`.
        let original_slot = unsafe { &*self.slot };

        // SAFETY: `current_ref_raw()` points to this CPU's per-CPU data. If the
        // task resumed on a different CPU, the scheduler restored `self.ptr` into
        // this current slot and the guard must unwind it to the previous nested
        // context there.
        let current_slot = unsafe { ACTIVE_EXCEPTION_CONTEXT_PTR.current_ref_raw() };

        if core::ptr::eq(current_slot, self.slot) {
            if let Err(_observed) = original_slot.compare_exchange(
                self.ptr,
                self.prev,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                // A scheduler handoff may already have cleared the slot. Keeping
                // the observed newer value is correct because this guard no
                // longer owns the CPU-visible active context.
            }
        } else {
            if let Err(_observed) = current_slot.compare_exchange(
                self.ptr,
                self.prev,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                // The current CPU may have no restored context if the task was
                // switched away again before the guard unwound. In that case the
                // scheduler-owned value must be preserved.
            } else {
                // `self.prev` belongs to the next-outer live guard in this task's
                // trap nesting chain. Restoring it on the migrated-to CPU is
                // required so the outer trap handler remains visible to
                // `in_exception_context()` after the inner guard drops. Clearing
                // to 0 here would incorrectly allow preemption while the outer
                // guard is still alive.
            }
            if let Err(_observed) = original_slot.compare_exchange(
                self.ptr,
                self.prev,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                // The original CPU slot is commonly cleared during switch-out.
                // CAS failure is therefore expected and means there is no stale
                // value owned by this guard left to restore.
            }
        }
    }
}

impl Drop for ExceptionContextGuard {
    #[inline]
    fn drop(&mut self) {
        self.restore_slot();
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

    #[def_test(serial)]
    fn test_suspend_resume_preserves_nested_context() {
        let outer = ExceptionContext::default();
        let inner = ExceptionContext::default();

        {
            let _outer_guard = ExceptionContextGuard::new(&outer);
            {
                let _inner_guard = ExceptionContextGuard::new(&inner);
                let suspended = suspend_active_exception_context();

                assert!(!in_exception_context());
                resume_active_exception_context(suspended);
                assert!(in_exception_context());
            }

            assert!(in_exception_context());
        }

        assert!(!in_exception_context());
    }
}
