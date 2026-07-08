// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process Management

#![no_std]
#![warn(missing_docs)]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod tests;

/// Capability-target validation helpers.
pub mod capability;
mod credentials;
/// Job-control query and mutation targets.
pub mod job_control;
mod lifecycle;
mod lookup;
/// PID-file-descriptor type and pidfd-related target resolution.
pub mod pidfd;
mod process;
/// Process and thread exit lifecycle owner operations.
pub mod process_exit;
mod process_group;
mod process_runtime;
/// Process-directed signal delivery and target resolution.
pub mod process_signals;
/// `/proc` visibility and task lookup helpers.
pub mod procfs;
mod publication;
/// Resource-limit target resolution.
pub mod resource_limits;
/// Scheduler-facing task, process, and group resolution.
pub mod scheduler;
mod session;
mod signal;
mod stat;
/// System-wide observable process/task views.
pub mod system_view;
mod thread;
mod timer_delivery;
/// Wait/reap helpers for process identity removal.
pub mod wait_reap;

#[macro_use]
extern crate klogger;

pub use credentials::{with_current_credentials, with_current_credentials_mut};
pub use kns::{CloneNsError, NamespaceFlags, NsProxy, UtsNamespace};
pub use kresources::ProcessResources;
pub use lifecycle::ProcessLifecycleState;
pub use pidfd::PidFd;
pub use posix_types::{Pid, Tid};
pub use process::{Process, ProcessExecUpdate, init_proc};
pub use process_group::ProcessGroup;
pub use process_runtime::ProcessForkConfig;
pub(crate) use process_runtime::{ProcessRuntime, fork_process_runtime};
pub use publication::PublishedUserTask;
pub use session::Session;
pub use stat::TaskStat;
#[cfg(feature = "tee")]
pub use tee_task_iface::{TeeSessionCtxTrait, TeeTaCtx};
pub use thread::{
    AsThread, CpuTimeState, CpuTimeStatistics, CurrentThread, PreparedUserClone, Thread,
    current_fs_context, current_task_name, current_user_process,
    current_user_process_address_space, current_user_process_fs_context, current_user_thread,
    current_user_tid, with_current_user_thread,
};
pub use timer_delivery::{
    dispatch_timer_delivery, init_timer_runtime, poll_cpu_timers, spawn_alarm_task,
};

pub(crate) fn allocate_thread_task_number(
    runtime: &alloc::sync::Arc<ProcessRuntime>,
) -> kerrno::KResult<alloc::sync::Arc<kidentity::PidHandle>> {
    kidentity::PidHandle::allocate_in(runtime.nsproxy().active_pid_ns())
}

/// Runtime action requested by a syscall after handling a user trap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UserThreadRuntimeAction {
    /// Continue with the normal post-syscall signal check.
    #[default]
    Continue,
    /// Skip the post-syscall signal check once (used by rt_sigreturn).
    SkipSignalCheckOnce,
}

/// Returns the current process-owned resources.
pub fn current_resources() -> alloc::sync::Arc<kresources::ProcessResources> {
    current_user_process()
        .resources()
        .expect("current user thread must still expose process resources")
}

/// Returns the current process umask.
pub fn current_umask() -> u32 {
    current_user_process()
        .umask()
        .expect("current user thread must still expose process umask")
}

/// Builds a futex key in the context of the current process address space.
pub fn current_futex_key(address: usize) -> kfutex::FutexKey {
    let aspace = current_user_process_address_space();
    let aspace = aspace.lock();
    kfutex::FutexKey::new(&aspace, address)
}

/// Registers the current unittest task as a published thread member of its process.
///
/// This is only for unittest scaffolding that temporarily installs a user-thread
/// runtime onto the current kernel task without going through the normal process
/// publication pipeline.
#[cfg(unittest)]
#[doc(hidden)]
pub fn publish_current_unittest_thread_membership() {
    let task = ktask::current().clone();
    let process = current_user_process();
    process.add_thread_task(&task);
}

/// Publishes and activates a fully prepared user task.
///
/// The caller must install the user-thread task extension before calling this
/// function. Publication completes before the task becomes runnable.
pub fn start_user_task(task: ktask::TaskInner) -> ktask::KtaskRef {
    publish_user_task(task).activate()
}

/// Publishes a fully prepared user task without making it runnable yet.
///
/// The caller must install the user-thread task extension before calling this
/// function. The returned handle is visible to process/task lookups but must be
/// explicitly committed via [`PublishedUserTask::commit`], activated directly,
/// or aborted. Dropping the handle before activation rolls back publication.
pub fn publish_user_task(task: ktask::TaskInner) -> PublishedUserTask {
    publication::prepare_user_task(task).publish()
}

/// Installs the init-process identity and thread runtime onto a prepared user task.
///
/// This constructs the stable init [`Process`], creates its initial [`Thread`],
/// and installs the user-thread task extension, but does not make the task
/// runnable yet. Callers may finish owner-specific setup such as stdio or TTY
/// binding before calling [`start_user_task`].
#[allow(clippy::too_many_arguments)]
pub fn install_init_process(
    task: &mut ktask::TaskInner,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<kfs::FsContext>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: kcred::Credentials,
) -> kerrno::KResult<alloc::sync::Arc<Process>> {
    let task_number = task
        .task_number()
        .cloned()
        .expect("init user task must already own a thread identity");
    let process = Process::new_init_with_task_number(task_number.clone());

    let thread = build_process_thread(
        process.clone(),
        task_number,
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        credentials,
    );

    // SAFETY: The freshly created `Thread` is uniquely owned here and is installed
    // exactly once as the task extension for the matching user task before the task
    // is spawned or made visible to any other observer.
    *task.task_ext_mut() = Some(unsafe { ktask::KTaskExt::from_impl(thread) });
    Ok(process)
}

/// Installs a user-thread task extension for an existing stable process identity.
///
/// This creates the process-shared runtime, binds a [`Thread`] to `task`, and
/// installs the canonical user-task extension payload, but does not publish or
/// run the task yet.
#[allow(clippy::too_many_arguments)]
pub fn install_process_thread(
    task: &mut ktask::TaskInner,
    process: alloc::sync::Arc<Process>,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<kfs::FsContext>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: kcred::Credentials,
) {
    let task_number = task
        .task_number()
        .cloned()
        .expect("install_process_thread requires a task-owned thread identity");
    let thread = build_process_thread(
        process,
        task_number,
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        credentials,
    );

    // SAFETY: The freshly created `Thread` is uniquely owned here and is installed
    // exactly once as the task extension for the matching user task before the task
    // is spawned or made visible to any other observer.
    *task.task_ext_mut() = Some(unsafe { ktask::KTaskExt::from_impl(thread) });
}

/// Builds a user thread bound to a freshly initialized process runtime.
///
/// This is the low-level constructor used when the caller must control the
/// exact `TaskExt` handoff sequence itself, such as temporary current-task
/// runtime installation in unittest scaffolding. Normal process creation paths
/// should prefer [`install_init_process`] or [`install_process_thread`].
#[allow(clippy::too_many_arguments)]
pub fn build_process_thread(
    process: alloc::sync::Arc<Process>,
    task_number: alloc::sync::Arc<kidentity::PidHandle>,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<kfs::FsContext>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: kcred::Credentials,
) -> alloc::boxed::Box<Thread> {
    build_process_thread_with_config(
        process,
        task_number,
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        credentials,
        process_runtime::ProcessRuntimeConfig::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_process_thread_with_config(
    process: alloc::sync::Arc<Process>,
    task_number: alloc::sync::Arc<kidentity::PidHandle>,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<kfs::FsContext>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: kcred::Credentials,
    config: process_runtime::ProcessRuntimeConfig,
) -> alloc::boxed::Box<Thread> {
    let runtime = ProcessRuntime::new(
        process.clone(),
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        credentials,
        config,
    );
    Thread::new(process, runtime, task_number)
}
