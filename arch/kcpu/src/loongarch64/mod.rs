// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 CPU context, trap, and userspace support.

#[macro_use]
mod macros;

mod ctx;
mod excp;
mod unaligned;

pub mod instrs;
pub use instrs as asm;
pub(crate) mod boot;
pub use boot::init_trap;

pub mod userspace;

pub use self::{
    ctx::{
        ExceptionContext as TrapFrame, ExceptionContext, FpuState, GeneralRegisters, TaskContext,
    },
    unaligned::UnalignedError,
};

#[cfg(all(unittest, target_arch = "loongarch64"))]
pub mod tests_arch {
    use kerrno::LinuxError;
    use unittest::def_test;

    use super::ExceptionContext;

    #[def_test]
    fn test_syscall_restart_ignored_without_syscall_trap() {
        let mut ctx = ExceptionContext::default();
        ctx.saved_syscall_arg0 = 7;
        ctx.set_ip(0x1004);
        ctx.set_retval((-LinuxError::ERESTARTSYS.into_raw() as isize) as usize);

        assert!(!ctx.is_from_syscall());
        assert!(ctx.syscall_restart_error().is_none());
        ctx.rollback_syscall();
        assert_eq!(ctx.ip(), 0x1004);
        assert_eq!(
            ctx.retval(),
            (-LinuxError::ERESTARTSYS.into_raw() as isize) as usize
        );
    }

    #[def_test]
    fn test_syscall_restart_after_syscall_trap() {
        let mut ctx = ExceptionContext::default();
        ctx.saved_syscall_arg0 = 7;
        ctx.set_ip(0x1004);
        ctx.set_retval((-LinuxError::ERESTARTSYS.into_raw() as isize) as usize);
        ctx.set_from_syscall(true);

        assert_eq!(ctx.syscall_restart_error(), Some(LinuxError::ERESTARTSYS));
        ctx.rollback_syscall();
        assert_eq!(ctx.retval(), 7);
        assert_eq!(ctx.ip(), 0x1000);
    }

    #[def_test]
    fn test_exec_reset_clears_gprs_and_syscall_state() {
        use super::userspace::UserContext;

        let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
        // Simulate a syscall trap carrying execve(path, argv, envp): the
        // third argument leaves a stale heap pointer in a2.
        uctx.set_arg2(0xdead);
        uctx.set_sysno(0xca11);
        uctx.set_retval(0xbaad);
        uctx.save_syscall_args();
        uctx.set_from_syscall(true);

        uctx.reset_for_exec();

        assert_eq!(uctx.arg2(), 0);
        assert_eq!(uctx.sysno(), 0);
        assert_eq!(uctx.retval(), 0);
        assert_eq!(uctx.regs.ra, 0);
        assert_eq!(uctx.regs.sp, 0);
        assert!(!uctx.is_from_syscall());
        // The caller re-establishes sp; ip is not owned by reset.
        assert_eq!(uctx.ip(), 0x1004);
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
