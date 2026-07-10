// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Init-process bootstrap owned by the process layer.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use fs_context::{copy_init_fs_struct, init_fs};
use kcred::Credentials;
use kexec::load_user_app;
use khal::uspace::UserContext;
use kidentity::allocate_root_pid_handle;
use kprocess::{UserThreadRuntimeAction, install_init_process, start_user_task};
use ksync::Mutex;
use ktty::tty::N_TTY;
use kvfs::{Filename, LookupFlags, LookupIntent};
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
    let loc = Filename::new(args[0].as_str())
        .lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Exec,
            LookupFlags::follow(),
        )
        .expect("Failed to resolve executable path");
    let path = loc
        .absolute_path()
        .expect("Failed to get executable absolute path");
    let name = loc.name();
    drop(fs);

    let (entry_vaddr, ustack_top) = load_user_app(&mut uspace, None, args, envs)
        .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);

    let task_number = allocate_root_pid_handle().expect("Failed to allocate init PID");
    let mut task = new_user_task(&name, uctx, 0, task_number, dispatch_syscall);
    task.ctx_mut()
        .set_page_table_root(uspace.page_table_hw_root());

    let fs_context = copy_init_fs_struct();
    let process = install_init_process(
        &mut task,
        path.to_string(),
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        fs_context,
        Arc::default(),
        Credentials::root(),
    )
    .expect("Failed to install init process");

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
