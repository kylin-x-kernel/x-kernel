// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

#[cfg(target_arch = "x86_64")]
use kerrno::KError;
use khal::uspace::UserContext;
use kprocess::UserThreadRuntimeAction;
use ksignal::{SignalAction, SignalDisposition, SignalInfo, Signo};
use linux_sysno::Sysno;
use osvm::VirtMutPtr;
use posix_types::k_siginfo;
use unittest::{assert_eq, def_test};
use unittest_support::TestUserBuffer;

use super::handle_syscall;

fn write_user_siginfo(buffer: &TestUserBuffer) -> usize {
    let info = SignalInfo::new_user(Signo::SIGUSR1, -1, 0);
    let pointer = buffer.as_user_ptr::<k_siginfo>();
    pointer.write_vm(k_siginfo::from(info)).unwrap();
    pointer as usize
}

fn ignore_sigusr1() {
    let thread = kprocess::current_user_thread();
    thread
        .process()
        .signal_manager()
        .unwrap()
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .expect("install test signal disposition");
}

#[def_test(user, serial)]
fn dispatch_rt_sigqueueinfo_ignores_the_trailing_register() {
    ignore_sigusr1();
    let thread = kprocess::current_user_thread();
    let tgid = thread.process().pid();
    let siginfo = TestUserBuffer::new(size_of::<k_siginfo>()).unwrap();
    let siginfo_address = write_user_siginfo(&siginfo);
    let mut uctx = UserContext::new(0, 0x8000usize.into(), 0);
    uctx.set_sysno(Sysno::rt_sigqueueinfo.id() as usize);
    uctx.set_arg0(tgid as usize);
    uctx.set_arg1(Signo::SIGUSR1 as usize);
    uctx.set_arg2(siginfo_address);
    // This ABI has only three arguments. A stale fourth register must not
    // be interpreted as a signal-set size or the siginfo pointer.
    uctx.set_arg3(usize::MAX);

    assert_eq!(handle_syscall(&mut uctx), UserThreadRuntimeAction::Continue);
    assert_eq!(uctx.retval(), 0);
}

#[def_test(user, serial)]
fn dispatch_rt_tgsigqueueinfo_uses_four_arguments() {
    ignore_sigusr1();
    let thread = kprocess::current_user_thread();
    let tgid = thread.process().pid();
    let tid = thread.pid();
    let siginfo = TestUserBuffer::new(size_of::<k_siginfo>()).unwrap();
    let siginfo_address = write_user_siginfo(&siginfo);
    let mut uctx = UserContext::new(0, 0x8000usize.into(), 0);
    uctx.set_sysno(Sysno::rt_tgsigqueueinfo.id() as usize);
    uctx.set_arg0(tgid as usize);
    uctx.set_arg1(tid as usize);
    uctx.set_arg2(Signo::SIGUSR1 as usize);
    uctx.set_arg3(siginfo_address);
    uctx.set_arg4(usize::MAX);

    assert_eq!(handle_syscall(&mut uctx), UserThreadRuntimeAction::Continue);
    assert_eq!(uctx.retval(), 0);
}

// The legacy epoll entries exist only in the x86_64 syscall table; this pins
// the number-to-handler mapping and the argument order of the wiring.
#[cfg(target_arch = "x86_64")]
#[def_test(user)]
fn dispatch_legacy_epoll_wait_maps_number_and_arguments() {
    let mut uctx = UserContext::new(0, 0x8000usize.into(), 0);
    uctx.set_sysno(Sysno::epoll_wait.id() as usize);
    uctx.set_arg0((-1i32) as usize); // invalid epfd must fail before anything else
    uctx.set_arg1(0);
    uctx.set_arg2(1);
    uctx.set_arg3(0);

    assert_eq!(handle_syscall(&mut uctx), UserThreadRuntimeAction::Continue);
    assert_eq!(
        uctx.retval(),
        KError::BadFileDescriptor.into_syscall_return() as usize
    );
}

// The other half of the legacy wiring: epoll_create(213) must reach the
// size check instead of the ENOSYS fallback.
#[cfg(target_arch = "x86_64")]
#[def_test(user)]
fn dispatch_legacy_epoll_create_maps_number_and_arguments() {
    let mut uctx = UserContext::new(0, 0x8000usize.into(), 0);
    uctx.set_sysno(Sysno::epoll_create.id() as usize);
    uctx.set_arg0(0); // a non-positive size is rejected before any allocation

    assert_eq!(handle_syscall(&mut uctx), UserThreadRuntimeAction::Continue);
    assert_eq!(
        uctx.retval(),
        KError::InvalidInput.into_syscall_return() as usize
    );
}
