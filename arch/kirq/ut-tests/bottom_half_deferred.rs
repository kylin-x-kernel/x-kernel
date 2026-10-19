// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
fn scoped_executor_restores_registered_hook_after_clear() {
    let _outer = super::ScopedDeferredExecutor::clear();
    assert!(register_deferred_executor(DeferredExecutorHooks {
        on_hardirq_exit: Some(first_hook),
    }));
    {
        let _inner = super::ScopedDeferredExecutor::clear();
        assert!(register_deferred_executor(DeferredExecutorHooks {
            on_hardirq_exit: Some(second_hook),
        }));
        clear_deferred_executor();
    }
    FIRST_HOOK_CALLS.store(0, Ordering::Relaxed);
    assert_eq!(
        run_hardirq_exit_deferred(DeferredRunContext::new(1, None)),
        DeferredRunResult::Ran
    );
    // The hook slot is globally visible, so IRQ tails on other CPUs can
    // invoke the restored executor concurrently with this check; the
    // scoped pin only masks interrupts on the CPU running the test.
    // Requiring our own call to have run is still a full check: first_hook
    // executes only through the published slot, so a misrestored or
    // cleared slot would leave the counter at zero.
    assert!(FIRST_HOOK_CALLS.load(Ordering::Relaxed) >= 1);
    assert!(!register_deferred_executor(DeferredExecutorHooks {
        on_hardirq_exit: Some(second_hook),
    }));
}

#[def_test(serial)]
fn test_deferred_executor_registration_and_run() {
    let _executor = super::ScopedDeferredExecutor::clear();
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
    let _executor = super::ScopedDeferredExecutor::clear();
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
    let _executor = super::ScopedDeferredExecutor::clear();
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
    let _executor = super::ScopedDeferredExecutor::clear();
    clear_deferred_executor();
    assert_eq!(
        run_hardirq_exit_deferred(DeferredRunContext::new(3, Some(4))),
        DeferredRunResult::NoExecutor
    );
}

#[def_test(serial)]
fn test_deferred_executor_runs_outside_registration_lock() {
    let _executor = super::ScopedDeferredExecutor::clear();
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
