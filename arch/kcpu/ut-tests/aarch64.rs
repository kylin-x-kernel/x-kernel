// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

#[def_test(serial)]
fn test_exception_context_user_mode() {
    use aarch64_cpu::registers::SPSR_EL1;

    // EL0t: interrupted context ran in user space.
    let mut ctx = ExceptionContext {
        spsr: SPSR_EL1::M::EL0t.value,
        ..Default::default()
    };
    assert!(ctx.user_mode());
    // EL1t / EL1h: interrupted context ran in the kernel.
    ctx.spsr = SPSR_EL1::M::EL1t.value;
    assert!(!ctx.user_mode());
    ctx.spsr = SPSR_EL1::M::EL1h.value;
    assert!(!ctx.user_mode());
    // The N condition flag outside M must not change the classification.
    // `Field` masks are unshifted; place N at its register position.
    let n_flag = SPSR_EL1::N.mask << SPSR_EL1::N.shift;
    ctx.spsr = n_flag;
    assert!(ctx.user_mode());
    ctx.spsr = n_flag | SPSR_EL1::M::EL1h.value;
    assert!(!ctx.user_mode());

    // The entry frame UserContext::new builds from the same SPSR entities
    // must classify as user mode.
    let uctx = super::userspace::UserContext::new(0x1000, va!(0x2000), 0);
    assert!(uctx.user_mode());

    // The tick-classification accessor must report these same frames
    // through the active-frame slot.
    ctx.spsr = SPSR_EL1::M::EL0t.value;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(true));
    }
    ctx.spsr = SPSR_EL1::M::EL1h.value;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(false));
    }
    assert_eq!(crate::active_exception_user_mode(), None);
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
