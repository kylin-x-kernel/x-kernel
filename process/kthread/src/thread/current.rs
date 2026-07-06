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
///
/// # Panics
///
/// The returned handle panics when dereferenced if the current task is not a user thread.
///
/// # Examples
///
/// ```rust,ignore
/// let thread = kthread::current_thread();
/// let pid = thread.pid();
/// ```
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
        .map(|thread| thread.proc_state.fs_context())
        .unwrap_or_else(|| kernel_fs_context().clone())
}

/// Returns the current process-owned filesystem context.
///
/// Use this helper only for process-only paths such as syscalls or POSIX
/// logic that require a current user thread. Kernel-task callers should use
/// `current_fs_context` instead.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
///
/// # Examples
///
/// ```rust,ignore
/// let fs_context = kthread::current_process_fs_context();
/// let _guard = fs_context.lock();
/// ```
pub fn current_process_fs_context() -> Arc<Mutex<FsContext>> {
    current_process_state().fs_context()
}

/// Executes a closure with the current user thread.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
///
/// # Examples
///
/// ```rust,ignore
/// let pid = kthread::with_current_thread(|thread| thread.pid());
/// ```
pub fn with_current_thread<R>(f: impl FnOnce(&Thread) -> R) -> R {
    let thread = current_thread();
    f(&thread)
}

/// Returns the current process state.
///
/// This helper requires the current task to be a user thread.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
///
/// # Examples
///
/// ```rust,ignore
/// let proc_state = kthread::current_process_state();
/// let pid = proc_state.proc.pid();
/// ```
pub fn current_process_state() -> Arc<ProcessState> {
    with_current_thread(|thread| thread.process_state().clone())
}
