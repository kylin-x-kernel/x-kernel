// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 CPU context, trap, and userspace support.

mod ctx;

pub mod instrs;

mod excp;

pub mod userspace;

use aarch64_cpu::{
    asm::barrier,
    registers::{CNTKCTL_EL1, ReadWriteable},
};

pub use self::ctx::{ExceptionContext as TrapFrame, ExceptionContext, FpState, TaskContext};

/// Enable EL0 access to generic timer registers used by user runtimes.
#[inline]
fn enable_user_timer_access() {
    CNTKCTL_EL1.modify(
        CNTKCTL_EL1::EL0PCTEN::TrappedNone
            + CNTKCTL_EL1::EL0VCTEN::TrappedNone
            + CNTKCTL_EL1::EL0VTEN::TrappedNone
            + CNTKCTL_EL1::EL0PTEN::TrappedNone,
    );
    barrier::isb(barrier::SY);
}

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the exception vector, and sets `TTBR0_EL1` to 0 to
/// block low address access.
pub fn init_trap() {
    crate::userspace_common::init_exception_table();
    enable_user_timer_access();
    unsafe extern "C" {
        fn exception_vector_base();
    }
    // SAFETY: `exception_vector_base` is defined in `excp.S` and aligned per ARM vector
    // table requirements. Writing zero to TTBR0_EL1 disables low-address translation.
    unsafe {
        karch::write_trap_vector_base(exception_vector_base as *const () as usize);
        karch::write_user_page_table(memaddr::PhysAddr::from(0usize).into());
    }
}

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

    #[def_test]
    fn test_user_context_restart_ignored_without_syscall_trap() {
        use kerrno::LinuxError;

        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        uctx.save_syscall_args();
        uctx.set_retval((-LinuxError::ERESTARTSYS.into_raw() as isize) as usize);

        assert!(!uctx.is_from_syscall());
        assert!(uctx.syscall_restart_error().is_none());
        uctx.rollback_syscall();
        assert_eq!(uctx.ip(), 0x1004);
        assert_eq!(
            uctx.retval(),
            (-LinuxError::ERESTARTSYS.into_raw() as isize) as usize
        );
    }

    #[def_test]
    fn test_user_context_restart_after_syscall_trap() {
        use kerrno::LinuxError;

        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        uctx.save_syscall_args();
        uctx.set_retval((-LinuxError::ERESTARTSYS.into_raw() as isize) as usize);
        uctx.set_from_syscall(true);

        assert_eq!(uctx.syscall_restart_error(), Some(LinuxError::ERESTARTSYS));
        uctx.rollback_syscall();
        assert_eq!(uctx.retval(), 7);
        assert_eq!(uctx.ip(), 0x1000);
    }

    #[def_test]
    fn test_exec_reset_clears_gprs_and_syscall_state() {
        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        // Simulate a syscall trap carrying execve(path, argv, envp): the
        // third argument leaves a stale heap pointer in x2.
        uctx.set_arg2(0xdead);
        uctx.set_sysno(0xca11);
        uctx.set_retval(0xbaad);
        uctx.save_syscall_args();
        uctx.set_from_syscall(true);

        uctx.reset_for_exec();

        assert_eq!(uctx.arg2(), 0);
        assert_eq!(uctx.sysno(), 0);
        assert_eq!(uctx.retval(), 0);
        assert!(!uctx.is_from_syscall());
        // Control fields owned by the caller are not overwritten.
        assert_eq!(uctx.ip(), 0x1004);
        assert_eq!(uctx.sp(), 0x2000);
    }

    #[def_test]
    fn test_exec_reset_entry_can_be_reestablished() {
        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        uctx.set_arg2(0xdead);
        uctx.save_syscall_args();
        uctx.set_from_syscall(true);

        uctx.reset_for_exec();

        // The execve caller re-establishes the ELF entry state afterwards.
        uctx.set_ip(0x2004);
        uctx.set_sp(0x3000);
        uctx.set_tls(0x88);

        assert_eq!(uctx.arg2(), 0);
        assert_eq!(uctx.ip(), 0x2004);
        assert_eq!(uctx.sp(), 0x3000);
        assert_eq!(uctx.tls(), 0x88);
    }
}
