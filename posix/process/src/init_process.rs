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
use kexec::{
    ExecRequest, PreparedExecImage, PrivateFileViewSpec, load_prepared_user_app,
    prepare_user_app_request,
};
use khal::uspace::UserContext;
use kprocess::{UserThreadRuntimeAction, build_process_thread, publish_user_task};
use ksync::Mutex;
use memaddr::VirtAddr;
use posix_fs::file::add_stdio;

use crate::runtime::run_user_thread_loop;

/// Installs the prepared init image into `uspace`, registering its private file
/// views for cache invalidation.
///
/// - `uspace`: the init address space.
/// - `prepared`: the prepared executable and interpreter image.
///
/// Returns: the image entry point and initial user stack pointer.
///
/// Locking: holds the address-space mutex while mapping and registering.
fn load_init_image(
    uspace: &Arc<Mutex<memspace::MmSpace>>,
    prepared: PreparedExecImage,
) -> (VirtAddr, VirtAddr) {
    let mut guard = uspace.lock();
    let observer = memspace::MmObserver::new(uspace);
    let view = PrivateFileViewSpec {
        mm_id: guard.mm_id(),
        observer: &observer,
        invalidate: guard.invalidate_handle(uspace),
    };
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the PID 1 image cannot be \
                  loaded into its address space; init spawning has no error return or recovery"
    )]
    load_prepared_user_app(&mut guard, prepared, Some(view))
        .unwrap_or_else(|e| panic!("Failed to load init image: {e}"))
}

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
/// Panics if `args` is empty, the allocated PID is not 1, or init process construction,
/// executable loading, publication, or stdio setup fails, or
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

    let uspace = Arc::new(Mutex::new(
        memspace::MmSpace::new_user_empty().expect("Failed to create user address space"),
    ));

    let cred = initial_cred();
    // Resolve the executable through the shared `ExecRequest` path so that the
    // metadata (basename / absolute path) and the actual load come from one and
    // the same source. The caller is a kernel thread without a user runtime, so
    // `current_fs_context()` falls back to the global `INIT_FS`.
    let init_path = args
        .first()
        .expect("init spawn requires a non-empty args vector with the exe path");
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the configured PID 1 \
                  executable cannot be resolved; init spawning has no error return or recovery"
    )]
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

    // Prepare without touching the unpublished MM, then install without any
    // destructive exec commitment or current-user-runtime requirement.
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the PID 1 executable image \
                  cannot be prepared; init spawning has no error return or recovery"
    )]
    let prepared = prepare_user_app_request(ExecRequest::from_resolved(
        binprm.location().clone(),
        binprm.args().to_vec(),
        binprm.envs().to_vec(),
        cred.clone(),
    ))
    .unwrap_or_else(|e| panic!("Failed to prepare init image: {e}"));
    let exec_saved_aux = prepared.saved_aux_words();
    let (entry_vaddr, ustack_top) = load_init_image(&uspace, prepared);

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let page_table_root = uspace.lock().page_table_hw_root();

    let fs_context = copy_init_fs_struct();
    let process = kprocess::Process::new_init_with_task_number(pid_handle.clone());
    process.set_saved_aux(exec_saved_aux);
    assert!(
        process.is_init(),
        "spawned init process must register as INIT_PROC"
    );
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: PID 1 thread preparation \
                  failed; init spawning has no error return or recovery"
    )]
    let thread = build_process_thread(
        process.clone(),
        pid_handle.clone(),
        path,
        Arc::new(args.to_vec()),
        uspace,
        fs_context,
        Default::default(),
        cred,
    )
    .unwrap_or_else(|error| panic!("Failed to prepare init thread: {error}"));

    {
        let fs_context_ref = process
            .fs_context()
            .expect("init process must have a live fs context");
        let fs_context = fs_context_ref.lock();
        thread
            .files()
            .with_fd_table(|table| add_stdio(&mut table.write(), &fs_context))
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
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: PID 1 cannot start without \
                  its entry owner; this bootstrap boundary has no recovery or error return"
    )]
    let entry = ktask::PreparedTaskEntry::new(entry).expect("Failed to prepare init entry");
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
        .expect("Failed to prepare init process publication")
        .commit(|_| Ok(()))
        .expect("Failed to publish init process");
}
