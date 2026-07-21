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
mod stat;
/// System-wide observable process/task views.
pub mod system_view;
mod thread;
mod timer_delivery;
/// Wait/reap helpers for process identity removal.
pub mod wait_reap;

#[macro_use]
extern crate klogger;

pub use credentials::{current_cred, current_real_cred};
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
    AsThread, CpuTimeState, CurrentThread, PreparedUserClone, Thread, current_fs_context,
    current_user_mm_id, current_user_process, current_user_process_address_space,
    current_user_process_fs_context, current_user_thread, current_user_tid,
    with_current_user_thread,
};
pub use timer_delivery::{
    dispatch_timer_delivery, init_timer_runtime, poll_cpu_timers, spawn_alarm_task,
};

pub(crate) fn allocate_thread_task_number()
-> kerrno::KResult<alloc::sync::Arc<kidentity::PidHandle>> {
    kidentity::allocate_root_pid_handle()
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

/// Publishes and activates a fully constructed user task.
///
/// Publication completes before the task becomes runnable.
pub fn start_user_task(task: ktask::TaskInner) -> ktask::KtaskRef {
    publish_user_task(task).activate()
}

/// Publishes a fully constructed user task without making it runnable yet.
///
/// The returned handle is visible to process/task lookups but must be
/// explicitly committed via [`PublishedUserTask::commit`], activated directly,
/// or aborted. Dropping the handle before activation rolls back publication.
pub fn publish_user_task(task: ktask::TaskInner) -> PublishedUserTask {
    publication::prepare_user_task(task).publish()
}

/// Builds a user thread bound to a freshly initialized process runtime.
///
/// Pass the returned thread to [`ktask::TaskInner::new_user`] so the task and
/// its runtime are constructed as one object.
#[allow(clippy::too_many_arguments)]
pub fn build_process_thread(
    process: alloc::sync::Arc<Process>,
    task_number: alloc::sync::Arc<kidentity::PidHandle>,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<fs_context::FsStruct>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: alloc::sync::Arc<kcred::Cred>,
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
    fs_context: alloc::sync::Arc<ksync::Mutex<fs_context::FsStruct>>,
    signal_actions: alloc::sync::Arc<ksync::spin::SpinNoIrq<ksignal::api::SignalActions>>,
    credentials: alloc::sync::Arc<kcred::Cred>,
    config: process_runtime::ProcessRuntimeConfig,
) -> alloc::boxed::Box<Thread> {
    let runtime = ProcessRuntime::new(
        process.clone(),
        exe_path,
        cmdline,
        address_space,
        fs_context,
        signal_actions,
        config,
    );
    Thread::new(process, runtime, task_number, credentials)
}
