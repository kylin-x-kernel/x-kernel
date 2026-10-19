// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
