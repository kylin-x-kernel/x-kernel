// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process identities, runtime capabilities and lifecycle publication.
//!
//! [`Process`] owns stable identity and parent/group/exit state; [`Thread`]
//! connects it to a task and process runtime. Clone owners start with
//! [`Thread::prepare_process_fork`] or [`Thread::prepare_thread_clone`], construct
//! the matching task, then [`publish_user_task`] and commit parent-side setup
//! before activation. [`process_exit`] and [`wait_reap`] coordinate teardown.
//! Scheduler/procfs callers use the corresponding semantic query modules.
//!
//! ## Staged publication example
//!
//! The caller supplies a current thread, validated fork policy and a user-entry
//! closure that runs the prepared image. After allocator/scheduler/MM setup:
//!
//! ```no_run
//! # fn prepare_child(
//! #     current: &kprocess::Thread,
//! #     config: kprocess::ProcessForkConfig,
//! #     entry: impl FnOnce() + Send + 'static,
//! # ) -> kerrno::KResult<ktask::KtaskRef> {
//! let prepared = current.prepare_process_fork(config)?;
//! let page_table_root = prepared.page_table_root();
//! let (thread, identity) = prepared.into_parts();
//! let mut task = ktask::TaskInner::new_user(entry, "child".into(), identity, thread);
//! task.ctx_mut().set_page_table_root(page_table_root);
//! let published = kprocess::publish_user_task(task)?;
//! // Perform fallible parent-side writeback in this closure before activation.
//! let runnable = published.commit(|_task| Ok(()))?;
//! # Ok(runnable)
//! # }
//! ```
//!
//! ## Scheduler values
//!
//! A pure value example needs no initialized process runtime:
//!
//! ```
//! use kprocess::NiceValue;
//! let nice = NiceValue::new_clamped(-50);
//! assert_eq!(nice.as_i32(), -20);
//! assert_eq!(nice.proc_stat_priority(), 0);
//! ```

#![no_std]
#![warn(missing_docs)]

extern crate alloc;

mod tests;

/// Capability-target validation helpers.
pub mod capability;
mod cgroup;
mod credentials;
/// Job-control query and mutation targets.
pub mod job_control;
mod lifecycle;
mod lookup;
/// PID-file-descriptor type and pidfd-related target resolution.
pub mod pidfd;
mod process;
mod process_domain;
/// Process and thread exit lifecycle owner operations.
pub mod process_exit;
mod process_group;
mod process_runtime;
/// Process-directed signal delivery and target resolution.
pub mod process_signals;
/// `/proc` visibility and task lookup helpers.
pub mod procfs;
/// Ptrace-style cross-task access checks.
pub mod ptrace;
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

pub use cgroup::{cgroup_member_process_ids, migrate_cgroup_process};
pub use credentials::{current_cred, current_real_cred};
pub use pidfd::PidFd;
pub use posix_types::{Pid, Tid};
pub use process::{
    LiveAddressSpace, Process, ProcessExecUpdate, ProcessExitPublication, init_proc,
};
pub use process_group::ProcessGroup;
pub(crate) use process_group::ProcessGroupMemberSlot;
pub use process_runtime::{
    ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions, ProcessForkConfig,
    replace_current_address_space,
};
pub(crate) use process_runtime::{ProcessRuntime, fork_process_runtime};
pub use publication::PublishedUserTask;
pub use session::{ControllingTerminal, Session, SetTerminalResult};
pub use stat::TaskStat;
#[cfg(feature = "tee")]
pub use tee_task_iface::{TeeSessionCtxTrait, TeeTaCtx};
pub use thread::{
    AsThread, CpuTimeState, CurrentThread, NiceValue, PreparedUserClone, SchedulerParameters,
    Thread, current_fs_context, current_user_mm_id, current_user_process,
    current_user_process_address_space, current_user_process_fs_context, current_user_thread,
    current_user_tid, with_current_user_thread,
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
    /// Continue with the normal post-syscall signal check and syscall restart.
    #[default]
    Continue,
    /// Signal return already decided whether the interrupted syscall restarts.
    ///
    /// The caller still delivers signals that became pending while the handler
    /// ran. It must not apply `restart_syscall` to the restored context.
    SkipSyscallRestart,
}

/// Returns the current process-owned resources.
///
/// # Panics
///
/// Panics outside a current user-thread runtime or when its process runtime
/// is no longer reachable.
pub fn current_resources() -> alloc::sync::Arc<kresources::ProcessResources> {
    current_user_process()
        .resources()
        .expect("current user thread must still expose process resources")
}

/// Returns the current process umask.
///
/// # Panics
///
/// Panics outside a current user-thread runtime or after its filesystem
/// context/runtime has been detached.
pub fn current_umask() -> u32 {
    current_user_process()
        .umask()
        .expect("current user thread must still expose process umask")
}

/// Publishes and activates a fully constructed user task.
///
/// Publication completes before the task becomes runnable.
///
/// # Panics
///
/// Panics if publication fails or its task identity/runtime invariants do
/// not hold. Use `publish_user_task` to handle fallible publication.
pub fn start_user_task(task: ktask::TaskInner) -> ktask::KtaskRef {
    publish_user_task(task)
        .expect("user task publication must succeed")
        .activate()
}

/// Publishes a fully constructed user task without making it runnable yet.
///
/// The returned handle is visible to process/task lookups but must be
/// explicitly committed via [`PublishedUserTask::commit`], activated directly,
/// or aborted. Dropping the handle before activation rolls back publication.
///
/// # Errors
///
/// Returns an error when a prepared thread cannot be reconciled with its
/// process's current cgroup before publication.
///
/// # Panics
///
/// Panics if the task lacks a matching Thread/PidHandle identity or violates
/// publication-slot or unpublished-cgroup membership invariants.
pub fn publish_user_task(task: ktask::TaskInner) -> kerrno::KResult<PublishedUserTask> {
    publication::prepare_user_task(task).publish()
}

/// Builds a user thread bound to a freshly initialized process runtime.
///
/// Pass the returned thread to [`ktask::TaskInner::new_user`] so the task and
/// its runtime are constructed as one object.
/// `signal_actions` is an owned initial configuration; use the thread clone
/// preparation APIs to share an existing process's signal configuration.
#[allow(clippy::too_many_arguments)]
pub fn build_process_thread(
    process: alloc::sync::Arc<Process>,
    task_number: alloc::sync::Arc<kidentity::PidHandle>,
    exe_path: alloc::string::String,
    cmdline: alloc::sync::Arc<alloc::vec::Vec<alloc::string::String>>,
    address_space: alloc::sync::Arc<ksync::Mutex<memspace::MmSpace>>,
    fs_context: alloc::sync::Arc<ksync::Mutex<fs_context::FsStruct>>,
    signal_actions: ksignal::api::SignalActions,
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
    signal_actions: ksignal::api::SignalActions,
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
