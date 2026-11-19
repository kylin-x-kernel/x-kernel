// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::LinuxError;
use unittest::def_test;

use super::ExceptionContext;

#[def_test]
fn idle_irq_return_skips_only_the_wait_instruction() {
    unsafe extern "C" {
        static __karch_idle_instruction: u8;
        static __karch_idle_return: u8;
    }
    let idle_pc = core::ptr::addr_of!(__karch_idle_instruction) as usize;
    let exit_pc = core::ptr::addr_of!(__karch_idle_return) as usize;
    let mut pc = idle_pc;
    karch::fixup_idle_irq_return(&mut pc);
    assert_eq!(pc, exit_pc);
    for other_pc in [0, idle_pc - 4, exit_pc, exit_pc + 4] {
        let mut pc = other_pc;
        karch::fixup_idle_irq_return(&mut pc);
        assert_eq!(pc, other_pc);
    }
}

#[def_test(serial)]
fn test_exception_context_user_mode() {
    use super::ctx::PPLV_UMODE;

    // PPLV = user: interrupted context ran in user mode.
    let mut ctx = ExceptionContext::default();
    ctx.prmd = PPLV_UMODE;
    assert!(ctx.user_mode());
    // PPLV = 0: interrupted context ran in the kernel.
    ctx.prmd = 0;
    assert!(!ctx.user_mode());
    // PIE (bit 2) outside PPLV must not change the classification.
    ctx.prmd = PPLV_UMODE | (1 << 2);
    assert!(ctx.user_mode());
    ctx.prmd = 1 << 2;
    assert!(!ctx.user_mode());

    // The entry frame UserContext::new builds from the same PPLV entity
    // must classify as user mode.
    let uctx = super::userspace::UserContext::new(0x1000, va!(0x2000), 0);
    assert!(uctx.user_mode());

    // The tick-classification accessor must report these same frames
    // through the active-frame slot.
    ctx.prmd = PPLV_UMODE;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(true));
    }
    ctx.prmd = 0;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(false));
    }
    assert_eq!(crate::active_exception_user_mode(), None);
}

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
