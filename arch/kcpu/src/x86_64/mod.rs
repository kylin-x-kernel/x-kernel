// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 CPU context, trap, and userspace support.

mod ctx;
mod gdt;
mod idt;

pub mod instrs;
pub use instrs as asm;
pub mod boot;

mod excp;

#[cfg(feature = "uspace")]
pub mod userspace;

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, ExtendedState, FxsaveArea, TaskContext,
};

#[cfg(all(unittest, target_arch = "x86_64"))]
pub mod tests_arch {
    use unittest::def_test;

    use super::ExceptionContext;

    #[def_test]
    fn test_exception_context_args() {
        let mut ctx = ExceptionContext::default();
        ctx.set_arg0(10);
        ctx.set_arg1(20);
        ctx.set_arg2(30);
        assert_eq!(ctx.arg0(), 10);
        assert_eq!(ctx.arg1(), 20);
        assert_eq!(ctx.arg2(), 30);
    }

    #[def_test]
    fn test_exception_context_ip_sp() {
        let mut ctx = ExceptionContext::default();
        ctx.set_ip(0x2000);
        ctx.set_sp(0x3000);
        assert_eq!(ctx.ip(), 0x2000);
        assert_eq!(ctx.sp(), 0x3000);
    }

    #[def_test]
    fn test_exception_context_sysno_retval() {
        let mut ctx = ExceptionContext::default();
        ctx.set_sysno(7);
        assert_eq!(ctx.sysno(), 7);
        assert_eq!(ctx.retval(), 7);
        ctx.set_retval(9);
        assert_eq!(ctx.sysno(), 9);
        assert_eq!(ctx.retval(), 9);
    }
}
