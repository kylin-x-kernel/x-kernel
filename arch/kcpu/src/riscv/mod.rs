// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V CPU context, trap, and userspace support.

#[macro_use]
mod macros;

mod ctx;
mod excp;

pub mod instrs;
pub use instrs as asm;
pub(crate) mod boot;

pub mod userspace;

pub use boot::init_trap;

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, FpState, GeneralRegisters, TaskContext,
};

#[cfg(all(unittest, any(target_arch = "riscv32", target_arch = "riscv64")))]
pub mod tests_arch {
    use kerrno::LinuxError;
    use unittest::def_test;

    use super::ExceptionContext;

    #[def_test]
    fn test_syscall_restart_ignored_without_syscall_trap() {
        let mut ctx = ExceptionContext {
            saved_syscall_arg0: 7,
            ..Default::default()
        };
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
        let mut ctx = ExceptionContext {
            saved_syscall_arg0: 7,
            ..Default::default()
        };
        ctx.set_ip(0x1004);
        ctx.set_retval((-LinuxError::ERESTARTSYS.into_raw() as isize) as usize);
        ctx.set_from_syscall(true);

        assert_eq!(ctx.syscall_restart_error(), Some(LinuxError::ERESTARTSYS));
        ctx.rollback_syscall();
        assert_eq!(ctx.retval(), 7);
        assert_eq!(ctx.ip(), 0x1000);
    }
}
