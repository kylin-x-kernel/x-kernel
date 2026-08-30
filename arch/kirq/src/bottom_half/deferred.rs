// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ-tail deferred-execution handoff.
//!
//! This module provides the first stable boundary for future IRQ bottom-half
//! work. It does not implement softirq vectors, workerqueue execution, or IRQ
//! threads; it only lets one owner install a bounded handoff that runs during
//! the common IRQ-on exit path after irqchip dispatch and after the generic
//! hardirq context depth has been dropped.

use core::sync::atomic::{AtomicUsize, Ordering};

use kspin::SpinNoIrq;

use crate::Virq;

/// Callback invoked from the common IRQ-on tail path.
///
/// The callback runs while the architecture adapter still has preemption
/// disabled, but after `kirq::context` has left hardirq context for that IRQ-on
/// exception. It must not sleep, allocate on the hot path, wait for
/// scheduler activity, assume a current process thread, or recursively call
/// [`run_hardirq_exit_deferred`].
pub type DeferredExecutorHook = fn(DeferredRunContext);

/// Optional hardirq-exit deferred executor callbacks.
///
/// Only one owner can register hooks at a time. The initial consumer is
/// expected to be the later softirq subsystem; workerqueue and threaded-IRQ
/// support should hang from that subsystem or from explicit future APIs, not
/// from implicit extra owners here.
#[derive(Clone, Copy, Default)]
pub struct DeferredExecutorHooks {
    /// Called after irqchip dispatch has returned and hardirq context ended.
    pub on_hardirq_exit: Option<DeferredExecutorHook>,
}

impl DeferredExecutorHooks {
    const fn has_executor(self) -> bool {
        self.on_hardirq_exit.is_some()
    }
}

/// Context passed to a hardirq-exit deferred executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredRunContext {
    vector: usize,
    resolved_irq: Option<Virq>,
}

impl DeferredRunContext {
    /// Creates a deferred run context for an IRQ-on exception.
    pub const fn new(vector: usize, resolved_irq: Option<Virq>) -> Self {
        Self {
            vector,
            resolved_irq,
        }
    }

    /// Returns the interrupt vector passed to the IRQ trap adapter.
    pub const fn vector(&self) -> usize {
        self.vector
    }

    /// Returns the OS-visible normal IRQ resolved for this exception, if any.
    ///
    /// This is an identity hint for IRQ-tail hooks, not a handled marker. A
    /// value can be present even when dispatch found no descriptor or handler.
    pub const fn resolved_irq(&self) -> Option<Virq> {
        self.resolved_irq
    }
}

/// Result of attempting to run the deferred hardirq-exit handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredRunResult {
    /// No executor is currently registered.
    NoExecutor,
    /// A registered executor was called.
    Ran,
}

static DEFERRED_EXECUTOR_HOOKS: SpinNoIrq<DeferredExecutorHooks> =
    SpinNoIrq::new(DeferredExecutorHooks {
        on_hardirq_exit: None,
    });
static DEFERRED_EXECUTOR_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the single deferred executor owner.
///
/// Returns `false` when another owner is already registered, or when `hooks`
/// does not contain an executor. Hook implementations run in hardirq-exit
/// context and must follow [`DeferredExecutorHook`]'s constraints.
pub fn register_deferred_executor(hooks: DeferredExecutorHooks) -> bool {
    if !hooks.has_executor() {
        return false;
    }

    let mut current = DEFERRED_EXECUTOR_HOOKS.lock();
    if current.has_executor() {
        return false;
    }
    *current = hooks;
    let Some(hook) = hooks.on_hardirq_exit else {
        return false;
    };
    DEFERRED_EXECUTOR_HOOK.store(hook as usize, Ordering::Release);
    true
}

/// Clear the registered deferred executor owner.
///
/// This is intended for shutdown and focused tests. Normal runtime ownership
/// should be established once by the subsystem that owns deferred IRQ work.
pub fn clear_deferred_executor() {
    DEFERRED_EXECUTOR_HOOK.store(0, Ordering::Release);
    *DEFERRED_EXECUTOR_HOOKS.lock() = DeferredExecutorHooks::default();
}

/// Run the normal hardirq-exit deferred handoff.
///
/// The runner reads the single published hook from an atomic slot and never
/// takes the registration lock on the IRQ-tail hot path. This function must not
/// be called from the dedicated IRQ-off NMI path. An NMI classified inside an
/// IRQ-on entry still reaches this common outer tail.
pub fn run_hardirq_exit_deferred(ctx: DeferredRunContext) -> DeferredRunResult {
    let hook = DEFERRED_EXECUTOR_HOOK.load(Ordering::Acquire);
    if hook == 0 {
        return DeferredRunResult::NoExecutor;
    }
    // SAFETY: `DEFERRED_EXECUTOR_HOOK` is only written from
    // `register_deferred_executor`, which stores a `DeferredExecutorHook`
    // function pointer as `usize`, or from `clear_deferred_executor`, which
    // stores zero. The zero case returned above, so the value here is a
    // function pointer previously published by registration.
    let hook: DeferredExecutorHook = unsafe { core::mem::transmute(hook) };

    hook(ctx);
    DeferredRunResult::Ran
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_deferred {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::{
        DeferredExecutorHooks, DeferredRunContext, DeferredRunResult, clear_deferred_executor,
        register_deferred_executor, run_hardirq_exit_deferred,
    };

    static FIRST_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SECOND_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CLEARING_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static TARGET_CONTEXT_CALLS: AtomicUsize = AtomicUsize::new(0);
    const CLEARING_HOOK_TEST_VECTOR: usize = 0x5eed;
    const CLEARING_HOOK_TEST_VIRQ: usize = 0x6eed;

    fn first_hook(ctx: DeferredRunContext) {
        FIRST_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        if ctx.vector() == 7 && ctx.resolved_irq() == Some(42) {
            TARGET_CONTEXT_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn second_hook(_ctx: DeferredRunContext) {
        SECOND_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn clearing_hook(ctx: DeferredRunContext) {
        if ctx.vector() == CLEARING_HOOK_TEST_VECTOR
            && ctx.resolved_irq() == Some(CLEARING_HOOK_TEST_VIRQ)
        {
            CLEARING_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
            clear_deferred_executor();
        }
    }

    #[def_test(serial)]
    fn test_deferred_executor_registration_and_run() {
        clear_deferred_executor();
        FIRST_HOOK_CALLS.store(0, Ordering::Relaxed);
        TARGET_CONTEXT_CALLS.store(0, Ordering::Relaxed);

        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(first_hook),
        }));

        assert_eq!(
            run_hardirq_exit_deferred(DeferredRunContext::new(7, Some(42))),
            DeferredRunResult::Ran
        );

        assert!(FIRST_HOOK_CALLS.load(Ordering::Relaxed) >= 1);
        assert_eq!(TARGET_CONTEXT_CALLS.load(Ordering::Relaxed), 1);
        clear_deferred_executor();
    }

    #[def_test(serial)]
    fn test_deferred_executor_rejects_duplicate_without_overwrite() {
        clear_deferred_executor();
        FIRST_HOOK_CALLS.store(0, Ordering::Relaxed);
        SECOND_HOOK_CALLS.store(0, Ordering::Relaxed);

        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(first_hook),
        }));
        assert!(!register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(second_hook),
        }));

        assert_eq!(
            run_hardirq_exit_deferred(DeferredRunContext::new(1, Some(2))),
            DeferredRunResult::Ran
        );
        assert!(FIRST_HOOK_CALLS.load(Ordering::Relaxed) >= 1);
        assert_eq!(SECOND_HOOK_CALLS.load(Ordering::Relaxed), 0);
        clear_deferred_executor();
    }

    #[def_test(serial)]
    fn test_deferred_executor_clear_allows_reregister() {
        clear_deferred_executor();
        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(first_hook),
        }));
        clear_deferred_executor();
        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(second_hook),
        }));
        clear_deferred_executor();
    }

    #[def_test(serial)]
    fn test_deferred_executor_noop_without_registration() {
        clear_deferred_executor();
        assert_eq!(
            run_hardirq_exit_deferred(DeferredRunContext::new(3, Some(4))),
            DeferredRunResult::NoExecutor
        );
    }

    #[def_test(serial)]
    fn test_deferred_executor_runs_outside_registration_lock() {
        clear_deferred_executor();
        CLEARING_HOOK_CALLS.store(0, Ordering::Relaxed);

        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(clearing_hook),
        }));
        assert_eq!(
            run_hardirq_exit_deferred(DeferredRunContext::new(
                CLEARING_HOOK_TEST_VECTOR,
                Some(CLEARING_HOOK_TEST_VIRQ)
            )),
            DeferredRunResult::Ran
        );
        assert_eq!(CLEARING_HOOK_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(
            run_hardirq_exit_deferred(DeferredRunContext::new(
                CLEARING_HOOK_TEST_VECTOR,
                Some(CLEARING_HOOK_TEST_VIRQ)
            )),
            DeferredRunResult::NoExecutor
        );
    }
}
