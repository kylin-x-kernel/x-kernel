// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Init-process bootstrap owned by the process layer.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use kcred::Credentials;
use kexec::load_user_app;
use kfs::{kernel_fs_context, new_process_fs_context};
use khal::uspace::UserContext;
use kprocess::{Pid, Process};
use ksync::Mutex;
use ktask::{KTaskExt, spawn_task};
use kthread::{
    ProcessState, ProcessStateConfig, Thread, UserThreadRuntimeAction, add_task_to_table,
};
use ktty::tty::N_TTY;
use posix_fs::file::add_stdio;

use crate::new_user_task;

/// Create, start, and wait for the initial user process.
pub fn run_init_process(
    args: &[String],
    envs: &[String],
    dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction + Send + 'static,
) -> i32 {
    let mut uspace =
        memspace::AddrSpace::new_user_empty().expect("Failed to create user address space");

    let loc = kernel_fs_context()
        .lock()
        .resolve(&args[0])
        .expect("Failed to resolve executable path");
    let path = loc
        .absolute_path()
        .expect("Failed to get executable absolute path");
    let name = loc.name();

    let (entry_vaddr, ustack_top) = load_user_app(&mut uspace, None, args, envs)
        .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);

    let mut task = new_user_task(name, uctx, 0, dispatch_syscall);
    task.ctx_mut()
        .set_page_table_root(uspace.page_table_hw_root());

    let pid = task.id().as_u64() as Pid;
    let proc = Process::new_init(pid);
    proc.add_thread(pid);

    N_TTY.bind_to(&proc).expect("Failed to bind ntty");

    let proc_state = ProcessState::new(
        proc,
        path.to_string(),
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        new_process_fs_context(),
        Arc::default(),
        None,
        Credentials::root(),
        ProcessStateConfig::default(),
    );
    {
        let fs_context = proc_state.fs_context().lock();
        add_stdio(&mut proc_state.resources.fd_table().write(), &fs_context)
            .expect("Failed to add stdio");
    }
    let thr = Thread::new(pid, proc_state);

    // SAFETY: The freshly created `Thread` is uniquely owned here and is installed
    // exactly once as the task extension for the matching user task before the task
    // is spawned or made visible to any other observer.
    *task.task_ext_mut() = Some(unsafe { KTaskExt::from_impl(thr) });

    let task = spawn_task(task);
    add_task_to_table(&task);

    // TODO: wait for all processes to finish
    let exit_code = task.join();
    if let Err(err) = kfs::sync_filesystems() {
        warn!("sync filesystems after init exit failed: {err:?}");
    }
    exit_code
}
