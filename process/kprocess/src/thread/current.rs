// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::ops::Deref;

use fs_context::{FsStruct, init_fs};
use ksync::Mutex;
use ktask::current;
use memspace::MmSpace;

use super::{AsThread, CurrentThread, Thread};
use crate::{Process, Tid};

impl Deref for CurrentThread {
    type Target = Thread;

    fn deref(&self) -> &Self::Target {
        self.0.as_thread()
    }
}

/// Returns the current user thread.
pub fn current_user_thread() -> CurrentThread {
    CurrentThread(current().clone())
}

/// Returns the current user-visible thread identifier.
pub fn current_user_tid() -> Tid {
    current_user_thread().tid()
}

/// Returns the current filesystem context for shared current-path helpers.
pub fn current_fs_context() -> Arc<Mutex<FsStruct>> {
    current()
        .try_as_thread()
        .map(|thread| {
            thread
                .process()
                .fs_context()
                .expect("current user thread must still expose process fs context")
        })
        .unwrap_or_else(init_fs)
}

/// Returns the current process-owned filesystem context.
///
/// # Panics
///
/// Panics if the current task is not a user thread or if the process runtime
/// has already detached its filesystem context, such as during late process exit.
pub fn current_user_process_fs_context() -> Arc<Mutex<FsStruct>> {
    current_user_process()
        .fs_context()
        .expect("current user thread must still expose process fs context")
}

/// Returns the current process-owned address space.
///
/// # Panics
///
/// Panics if the current task is not a user thread or if the process runtime
/// has already detached its address space, such as during late process exit.
pub fn current_user_process_address_space() -> Arc<Mutex<MmSpace>> {
    current_user_process()
        .address_space()
        .expect("current user thread must still expose process address space")
}

/// Returns the current process address-space identity without taking the aspace lock.
///
/// # Panics
///
/// Panics if the current task is not a user thread or if the process runtime
/// has already detached.
pub fn current_user_mm_id() -> u64 {
    current_user_process()
        .mm_id()
        .expect("current user thread must still expose process address space")
}

/// Returns the current stable process identity.
pub fn current_user_process() -> Arc<Process> {
    with_current_user_thread(|thread| thread.process().clone())
}

/// Executes a closure with the current user thread.
pub fn with_current_user_thread<R>(f: impl FnOnce(&Thread) -> R) -> R {
    let thread = current_user_thread();
    f(&thread)
}
