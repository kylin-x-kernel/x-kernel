// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::ExceptionContext;

#[def_test(serial)]
fn scoped_slot_access_restores_irq_mask_on_nested_error_return() {
    let was_enabled = karch::local_irq_enabled();
    let result = ACTIVE_EXCEPTION_CONTEXT_PTR.with_current_irq_disabled(|outer| {
        assert!(!karch::local_irq_enabled());
        ACTIVE_EXCEPTION_CONTEXT_PTR.with_current_irq_disabled(|inner| {
            assert!(core::ptr::eq(outer, inner));
            assert!(!karch::local_irq_enabled());
        });
        assert!(!karch::local_irq_enabled());
        Err::<(), _>(outer.load(Ordering::Relaxed))
    });
    assert!(result.is_err());
    assert_eq!(karch::local_irq_enabled(), was_enabled);
}

#[def_test(serial)]
fn exception_query_preserves_irq_mask() {
    let was_enabled = karch::local_irq_enabled();
    let was_active = in_exception_context();
    assert_eq!(karch::local_irq_enabled(), was_enabled);
    let irq_state = karch::save_irq_and_disable();
    let masked_active = in_exception_context();
    // SAFETY: ordinary IRQs remain masked until restore_irq below.
    assert_eq!(unsafe { in_exception_context_irqoff() }, masked_active);
    let remains_masked = !karch::local_irq_enabled();
    karch::restore_irq(irq_state);
    assert_eq!(masked_active, was_active);
    assert!(remains_masked);
    assert_eq!(karch::local_irq_enabled(), was_enabled);
}

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
        let irq_state = karch::save_irq_and_disable();
        // SAFETY: this query runs before restoring the saved IRQ state.
        let is_active = unsafe { in_exception_context_irqoff() };
        karch::restore_irq(irq_state);
        assert!(is_active);
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
