// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 CPU context, trap, and userspace support.

mod ctx;

pub mod boot;
pub mod instrs;

mod excp;

#[cfg(feature = "uspace")]
pub mod userspace;

pub use self::ctx::{ExceptionContext as TrapFrame, ExceptionContext, FpState, TaskContext};

#[cfg(all(unittest, target_arch = "aarch64"))]
pub mod tests_arch {
    use unittest::def_test;

    use super::ExceptionContext;

    #[def_test]
    fn test_exception_context_args() {
        let mut ctx = ExceptionContext::default();
        ctx.set_arg0(1);
        ctx.set_arg1(2);
        ctx.set_arg2(3);
        assert_eq!(ctx.arg0(), 1);
        assert_eq!(ctx.arg1(), 2);
        assert_eq!(ctx.arg2(), 3);
    }

    #[def_test]
    fn test_exception_context_ip_sysno() {
        let mut ctx = ExceptionContext::default();
        ctx.set_ip(0x1000);
        ctx.set_sysno(42);
        assert_eq!(ctx.ip(), 0x1000);
        assert_eq!(ctx.sysno(), 42);
    }

    #[def_test]
    fn test_exception_context_retval() {
        let mut ctx = ExceptionContext::default();
        ctx.set_retval(0x55aa);
        assert_eq!(ctx.retval(), 0x55aa);
    }
}
