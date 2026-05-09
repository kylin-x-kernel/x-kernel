// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};
use core::ops::Deref;

use kfs::{FsContext, kernel_fs_context};
use ksync::Mutex;
use ktask::current;

use super::{AsThread, CurrentThread, Thread};
use crate::ProcessState;

impl Deref for CurrentThread {
    type Target = Thread;

    fn deref(&self) -> &Self::Target {
        self.0.as_thread()
    }
}

/// Returns the current user thread.
pub fn current_thread() -> CurrentThread {
    CurrentThread(current().clone())
}

/// Returns the current task name.
pub fn current_task_name() -> String {
    current().name()
}

/// Returns the current filesystem context for shared current-path helpers.
///
/// Shared subsystems should use this helper when they only need a usable
/// path-resolution context. User threads get their process-owned context,
/// while kernel tasks fall back to the kernel-default context.
pub fn current_fs_context() -> Arc<Mutex<FsContext>> {
    current()
        .try_as_thread()
        .map(|thread| thread.proc_state.fs_context().clone())
        .unwrap_or_else(|| kernel_fs_context().clone())
}

/// Returns the current process-owned filesystem context.
///
/// Use this helper only for process-only paths such as syscalls or POSIX
/// logic that require a current user thread. Kernel-task callers should use
/// `current_fs_context` instead.
pub fn current_process_fs_context() -> Arc<Mutex<FsContext>> {
    current_process_state().fs_context().clone()
}

/// Executes a closure with the current user thread.
pub fn with_current_thread<R>(f: impl FnOnce(&Thread) -> R) -> R {
    let thread = current_thread();
    f(&thread)
}

/// Returns the current process state.
///
/// This helper requires the current task to be a user thread.
pub fn current_process_state() -> Arc<ProcessState> {
    with_current_thread(|thread| thread.process_state().clone())
}
