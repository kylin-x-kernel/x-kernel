// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ksignal::{
    SignalAction,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn interrupted_context(error: LinuxError) -> UserContext {
    let mut uctx = UserContext::new(0x1004, 0x2000usize.into(), 7);
    uctx.save_syscall_args();
    uctx.set_sysno(Sysno::read as usize);
    uctx.set_retval((-(error.into_raw() as isize)) as usize);
    uctx
}

#[def_test]
fn default_continue_does_not_suppress_syscall_restart() {
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let signal = ThreadSignalManager::new(1, process).unwrap();
    let mut uctx = interrupted_context(LinuxError::ERESTARTSYS);
    let action = signal.handle_signal(
        &mut uctx,
        SignalSet::default(),
        &SignalInfo::new_kernel(Signo::SIGCONT),
        &SignalAction::default(),
    );
    assert_eq!(action, Some(SignalOsAction::Continue));
    let has_installed_handler = matches!(action, Some(SignalOsAction::Handler));
    restart_syscall_without_handler(
        &mut uctx,
        UserThreadRuntimeAction::Continue,
        has_installed_handler,
    );
    assert!(uctx.syscall_restart_error().is_none());
    assert!(uctx.ip() < 0x1004);
}

#[def_test]
fn handler_and_sigreturn_contexts_are_not_restarted_again() {
    for (runtime_action, has_installed_handler) in [
        (UserThreadRuntimeAction::Continue, true),
        (UserThreadRuntimeAction::SkipSyscallRestart, false),
        (UserThreadRuntimeAction::SkipSyscallRestart, true),
    ] {
        let mut uctx = interrupted_context(LinuxError::ERESTARTSYS);
        restart_syscall_without_handler(&mut uctx, runtime_action, has_installed_handler);
        assert_eq!(uctx.ip(), 0x1004);
        assert_eq!(uctx.syscall_restart_error(), Some(LinuxError::ERESTARTSYS));
    }
}

#[def_test(user, serial)]
fn do_exit_retains_namespaces_until_reap() {
    let parent = kprocess::init_proc();
    let child = parent.fork(9_100);
    let number = kidentity::PidHandle::fixed_root(9_100);
    let aspace = Arc::new(ksync::Mutex::new(
        memspace::MmSpace::new_user_empty().expect("user mmspace should allocate"),
    ));
    let fs = fs_context::copy_init_fs_struct();
    let thread = kprocess::build_process_thread(
        child.clone(),
        number.clone(),
        alloc::string::String::from("exit-ns"),
        Arc::new(alloc::vec::Vec::new()),
        aspace,
        fs,
        ksignal::api::SignalActions::default(),
        kcred::initial_cred(),
    )
    .expect("thread build should succeed");
    let task = ktask::TaskInner::new_user(
        ktask::PreparedTaskEntry::new_nowait(|| crate::do_exit(0, false))
            .expect("test entry preparation"),
        alloc::string::String::from("exit-ns"),
        number,
        thread,
    );
    let task = kprocess::start_user_task(task);
    for _ in 0..100_000 {
        if task.state() == ktask::TaskState::Exited {
            break;
        }
        ktask::yield_now();
    }
    assert_eq!(task.state(), ktask::TaskState::Exited);
    child
        .mnt_ns()
        .expect("do_exit must retain namespaces until reap");
    kprocess::wait_reap::assert_reap_zombie_process(&child);
    assert!(
        child.mnt_ns().is_err(),
        "reap must release the retained namespace owner"
    );
}
