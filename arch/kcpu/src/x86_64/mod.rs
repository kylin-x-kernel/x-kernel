// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 CPU context, trap, and userspace support.

mod ctx;
mod gdt;
mod idt;

pub mod instrs;
pub use instrs as asm;
pub use karch::hypercall;
pub(crate) mod boot;
pub use boot::init_trap;

mod excp;

pub mod userspace;

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, ExtendedState, FxStateBlock, TaskContext,
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
        assert_eq!(ctx.orig_sysno(), 7);
        assert_eq!(ctx.retval(), 7);
        ctx.set_retval(9);
        assert_eq!(ctx.sysno(), 7);
        assert_eq!(ctx.orig_sysno(), 7);
        assert_eq!(ctx.retval(), 9);
    }

    #[def_test]
    fn test_exception_context_rollback_syscall() {
        let mut ctx = ExceptionContext::default();
        ctx.set_sysno(202);
        ctx.set_ip(0x1002);
        ctx.set_retval((-512isize) as usize);
        ctx.rollback_syscall();
        assert_eq!(ctx.retval(), 202);
        assert_eq!(ctx.ip(), 0x1000);
    }

    #[def_test]
    fn test_exec_reset_clears_gprs() {
        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        // Simulate a syscall trap carrying execve(path, argv, envp): the
        // third argument leaves a stale heap pointer in rdx.
        uctx.rax = 0xbaad;
        uctx.rcx = 1;
        uctx.rdx = 0x4004_f000;
        uctx.rbx = 2;
        uctx.rbp = 3;
        uctx.rsi = 4;
        uctx.rdi = 5;
        uctx.r8 = 6;
        uctx.r9 = 7;
        uctx.r10 = 8;
        uctx.r11 = 9;
        uctx.r12 = 10;
        uctx.r13 = 11;
        uctx.r14 = 12;
        uctx.r15 = 13;
        uctx.orig_rax = 0x3b; // execve syscall number
        uctx.gs_base = 0x1234;

        uctx.reset_for_exec();

        assert_eq!(uctx.rax, 0);
        assert_eq!(uctx.rcx, 0);
        assert_eq!(uctx.rdx, 0);
        assert_eq!(uctx.rbx, 0);
        assert_eq!(uctx.rbp, 0);
        assert_eq!(uctx.rsi, 0);
        assert_eq!(uctx.rdi, 0);
        assert_eq!(uctx.r8, 0);
        assert_eq!(uctx.r9, 0);
        assert_eq!(uctx.r10, 0);
        assert_eq!(uctx.r11, 0);
        assert_eq!(uctx.r12, 0);
        assert_eq!(uctx.r13, 0);
        assert_eq!(uctx.r14, 0);
        assert_eq!(uctx.r15, 0);
        assert_eq!(uctx.orig_rax, u64::MAX);
        assert_eq!(uctx.gs_base, 0);
        // Control fields owned by the caller are not overwritten.
        assert_eq!(uctx.rip, 0x1004);
        assert_eq!(uctx.rsp, 0x2000);
        assert_eq!(uctx.fs_base, 0);
    }

    #[def_test]
    fn test_exec_reset_entry_can_be_reestablished() {
        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        uctx.rdx = 0x4004_f000;
        uctx.reset_for_exec();

        // The execve caller re-establishes the ELF entry state afterwards.
        uctx.set_ip(0x2004);
        uctx.set_sp(0x3000);
        uctx.set_tls(0);

        assert_eq!(uctx.rdx, 0);
        assert_eq!(uctx.rip, 0x2004);
        assert_eq!(uctx.rsp, 0x3000);
    }
}
