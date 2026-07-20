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
}
