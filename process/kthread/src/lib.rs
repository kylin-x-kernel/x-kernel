// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-side thread/runtime surface.
//!
//! This crate owns the process-side thread/runtime surface:
//! thread state, shared process state, task registries, and signal-delivery
//! glue. Consumers should depend on this crate directly.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
#[macro_use]
extern crate klogger;

mod lifecycle_state;
mod posix_state;
mod process_state;
mod registry;
mod runtime_state;
mod signal;
mod stat;
mod thread;
mod timer;

pub use kfutex::{FutexEntry, FutexGuard, FutexKey, FutexTable, WaitQueue};
pub use kresources::ProcessResources;
pub use krlimit::{FILE_LIMIT, Rlimit, Rlimits};
pub use lifecycle_state::ProcessLifecycleState;
pub use posix_state::ProcessPosixState;
pub use process_state::{ProcessState, ProcessStateConfig};
pub use registry::{
    add_task_to_table, cleanup_task_tables, get_process_group, get_process_state, get_session,
    get_task, processes, tasks,
};
pub use runtime_state::ProcessRuntimeState;
pub use signal::{
    poll_timer, send_signal_to_process, send_signal_to_process_group, send_signal_to_thread,
    set_timer_state,
};
pub use stat::TaskStat;
#[cfg(feature = "tee")]
pub use tee_task_iface::{TeeSessionCtxTrait, TeeTaCtx};
pub use thread::{
    AsThread, CurrentThread, Thread, current_fs_context, current_process_fs_context,
    current_process_state, current_task_name, current_thread, with_current_thread,
};
pub use timer::{TimeManager, TimerState, spawn_alarm_task};

/// Returns the current process-owned resources.
pub fn current_resources() -> Arc<ProcessResources> {
    current_process_state().resources.clone()
}

/// Builds a futex key in the context of the current process address space.
pub fn current_futex_key(address: usize) -> FutexKey {
    let proc_state = current_process_state();
    let aspace = proc_state.address_space().lock();
    FutexKey::new(&aspace, address)
}
