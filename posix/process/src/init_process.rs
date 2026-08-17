// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Init-process bootstrap owned by the process layer.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use fs_context::copy_init_fs_struct;
use kcred::initial_cred;
use kexec::{ExecRequest, load_user_app_request};
use khal::uspace::UserContext;
use kprocess::{UserThreadRuntimeAction, build_process_thread, publish_user_task};
use ksync::Mutex;
use posix_fs::file::add_stdio;

use crate::runtime::run_user_thread_loop;

/// Spawns the initial user program as a fresh PID 1 user task.
///
/// This allocates the PID 1 root handle (it must be the first root PID
/// allocation in the system, which is guaranteed by running this from the
/// PID-less late-init bootstrap thread), builds a complete process and thread
/// runtime around it, and activates a new user task with the default
/// all-online-CPU affinity. The task enters user space on its own kernel stack
/// through the normal scheduler switch-in path, the same one fork uses.
///
/// Unlike the old in-place "transform current into init" model, this does **not**
/// touch the caller: it follows the FreeBSD-style "the bootstrap thread forks
/// init" model. The caller is expected to be a kernel thread (typically the
/// late-init bootstrap thread) and remains free to continue or exit after the
/// spawn.
///
/// `after_init_exit` runs on the spawned init task after its user loop exits and
/// is expected to perform system shutdown or another non-returning terminal
/// action.
///
/// # Panics
///
/// Panics if the allocated PID is not 1, if init process construction,
/// executable loading, publication, terminal binding, or stdio setup fails, or
/// if the spawned process does not register as the global init.
pub fn spawn_init_process(
    args: &[String],
    envs: &[String],
    dispatch_syscall: impl FnMut(&mut UserContext) -> UserThreadRuntimeAction + Send + 'static,
    after_init_exit: impl FnOnce() + Send + 'static,
) {
    // PID 1: must be the first root PID allocation in the system. The late-init
    // bootstrap thread holds an `Internal` identity and allocates none, so this
    // call naturally receives root_nr 1.
    let pid_handle =
        kidentity::allocate_root_pid_handle().expect("failed to allocate init PID handle");
    assert_eq!(pid_handle.root_nr(), 1, "init must be the first root PID");

    let mut uspace =
        memspace::MmSpace::new_user_empty().expect("Failed to create user address space");

    let cred = initial_cred();
    // Resolve the executable through the shared `ExecRequest` path so that the
    // metadata (basename / absolute path) and the actual load come from one and
    // the same source. The caller is a kernel thread without a user runtime, so
    // `current_fs_context()` falls back to the global `INIT_FS`.
    let init_path = args
        .first()
        .expect("init spawn requires a non-empty args vector with the exe path");
    let binprm = ExecRequest::from_path(
        init_path.clone(),
        args.to_vec(),
        envs.to_vec(),
        cred.clone(),
    )
    .prepare()
    .unwrap_or_else(|e| panic!("Failed to resolve init executable: {e}"));
    let name = binprm.location().name().to_string();
    let path = binprm
        .location()
        .absolute_path()
        .map(|p| p.to_string())
        .unwrap_or_else(|_| init_path.clone());

    // Load the image through the same loader entry point used by exec, reusing
    // the resolved location (and the args/envs already cloned into `binprm`) to
    // avoid a redundant lookup and a second full copy of args/envs.
    let (entry_vaddr, ustack_top) = load_user_app_request(
        &mut uspace,
        ExecRequest::from_resolved(
            binprm.location().clone(),
            binprm.args().to_vec(),
            binprm.envs().to_vec(),
            cred.clone(),
        ),
    )
    .unwrap_or_else(|e| panic!("Failed to load init image: {e}"));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let page_table_root = uspace.page_table_hw_root();

    let fs_context = copy_init_fs_struct();
    let process = kprocess::Process::new_init_with_task_number(pid_handle.clone());
    assert!(
        process.is_init(),
        "spawned init process must register as INIT_PROC"
    );
    let thread = build_process_thread(
        process.clone(),
        pid_handle.clone(),
        path,
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        fs_context,
        Arc::default(),
        cred,
    );

    {
        let fs_context_ref = process
            .fs_context()
            .expect("init process must have a live fs context");
        let fs_context = fs_context_ref.lock();
        add_stdio(
            &mut process
                .resources()
                .expect("init process must have live resources")
                .fd_table()
                .expect("init process must have a live fd table")
                .write(),
            &fs_context,
        )
        .expect("Failed to add stdio");
    }

    // Build a fresh user task carrying the runtime from construction (no
    // in-place install). The entry closure runs the user loop on the spawned
    // task's own kernel stack, then performs the post-init shutdown.
    let entry = move || {
        run_user_thread_loop(uctx, 0, dispatch_syscall);
        after_init_exit();
        ktask::exit(0);
    };
    let mut task = ktask::TaskInner::new_user(entry, name, pid_handle, thread);
    // Seed the saved context with init's page-table root; the scheduler writes
    // it to the hardware register on first switch-in, same as fork does.
    task.ctx_mut().set_page_table_root(page_table_root);
    // Keep the default all-online-CPU affinity from `new_user`. Clone inherits
    // the creator's mask, so pinning PID 1 to the boot CPU would freeze every
    // later user process (and `get_nprocs()` via sched_getaffinity) onto one
    // CPU. Secondary run queues are already registered when init is spawned.

    // Publish and activate through the standard fork path (caller-agnostic).
    publish_user_task(task)
        .commit(|_| Ok(()))
        .expect("Failed to publish init process");
}
