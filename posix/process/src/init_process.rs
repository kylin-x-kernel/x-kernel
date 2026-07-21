// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Init-process bootstrap owned by the process layer.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use fs_context::{copy_init_fs_struct, init_fs};
use kcred::initial_cred;
use kexec::load_user_app;
use khal::uspace::UserContext;
use kidentity::allocate_root_pid_handle;
use kprocess::{Process, UserThreadRuntimeAction, build_process_thread, start_user_task};
use ksync::Mutex;
use ktty::tty::N_TTY;
use kvfs::{Filename, LookupFlags, LookupIntent, Permission};
use posix_fs::file::add_stdio;

use crate::new_user_task;

/// Create, start, and wait for the initial user process.
pub fn run_init_process(
    args: &[String],
    envs: &[String],
    dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction + Send + 'static,
) -> i32 {
    let mut uspace =
        memspace::MmSpace::new_user_empty().expect("Failed to create user address space");

    let fs_guard = init_fs();
    let fs = fs_guard.lock();
    let cred = initial_cred();
    let loc = Filename::new(args[0].as_str())
        .lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Exec,
            LookupFlags::follow(),
            &cred,
        )
        .expect("Failed to resolve executable path");
    loc.permission(Permission::MAY_EXEC, &cred)
        .expect("Init executable is not executable");
    let path = loc
        .absolute_path()
        .expect("Failed to get executable absolute path");
    let name = loc.name();
    drop(fs);

    let (entry_vaddr, ustack_top) = load_user_app(&mut uspace, None, args, envs, cred.clone())
        .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let page_table_root = uspace.page_table_hw_root();

    let task_number = allocate_root_pid_handle().expect("Failed to allocate init PID");
    let process = Process::new_init_with_task_number(task_number.clone());
    let fs_context = copy_init_fs_struct();
    let thread = build_process_thread(
        process.clone(),
        task_number.clone(),
        path.to_string(),
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        fs_context,
        Arc::default(),
        cred,
    );
    let mut task = new_user_task(&name, uctx, 0, task_number, thread, dispatch_syscall);
    task.ctx_mut().set_page_table_root(page_table_root);

    N_TTY.bind_to(&process).expect("Failed to bind ntty");

    {
        let fs_struct = process
            .fs_context()
            .expect("init process must expose filesystem context");
        let fs_struct = fs_struct.lock();
        let resources = process
            .resources()
            .expect("init process must expose resources");
        add_stdio(&mut resources.fd_table().write(), &fs_struct).expect("Failed to add stdio");
    }
    let task = start_user_task(task);

    // TODO: wait for all processes to finish
    let exit_code = task.join();
    if let Err(err) = kvfs::sync_filesystems() {
        warn!("sync filesystems after init exit failed: {err:?}");
    }
    exit_code
}
