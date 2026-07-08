// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Init-process bootstrap owned by the process layer.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use kcred::Credentials;
use kexec::{ExecRequest, load_user_app_request};
use kfs::{kernel_fs_context, new_process_fs_context};
use khal::uspace::UserContext;
use kidentity::allocate_root_pid_handle;
use kprocess::{UserThreadRuntimeAction, install_init_process, start_user_task};
use ksync::Mutex;
use ktty::tty::N_TTY;
use kvfs::{LookupFlags, LookupIntent, lookup_location};
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

    let fs = kernel_fs_context().lock();
    let loc = lookup_location(
        &fs.lookup_context(),
        args[0].as_str(),
        LookupIntent::Exec,
        LookupFlags::follow(),
    )
    .expect("Failed to resolve executable path");
    let path = loc
        .absolute_path()
        .expect("Failed to get executable absolute path");
    let name = loc.name().to_string();
    drop(fs);

    let (entry_vaddr, ustack_top) = load_user_app_request(
        &mut uspace,
        ExecRequest::from_resolved_with_display(
            loc,
            path.to_string(),
            args.to_vec(),
            envs.to_vec(),
        ),
    )
    .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);

    let init_task_number = allocate_root_pid_handle().expect("failed to allocate init pid handle");
    let mut task = new_user_task(name.as_str(), uctx, 0, init_task_number, dispatch_syscall);
    task.ctx_mut()
        .set_page_table_root(uspace.page_table_hw_root());

    let proc = install_init_process(
        &mut task,
        path.to_string(),
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        new_process_fs_context(),
        Arc::default(),
        Credentials::root(),
    )
    .expect("failed to install init process identity");

    N_TTY.bind_to(&proc).expect("Failed to bind ntty");
    {
        let fs_context_ref = proc
            .fs_context()
            .expect("init process must have a live fs context");
        let fs_context = fs_context_ref.lock();
        add_stdio(
            &mut proc
                .resources()
                .expect("init process must have live resources")
                .fd_table()
                .write(),
            &fs_context,
        )
        .expect("Failed to add stdio");
    }

    let task = start_user_task(task);

    // TODO: wait for all processes to finish
    let exit_code = task.join();
    if let Err(err) = kfs::sync_filesystems() {
        warn!("sync filesystems after init exit failed: {err:?}");
    }
    exit_code
}
