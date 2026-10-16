// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::ops::Deref;

use fs_context::{FsStruct, init_fs};
use kcred::Cred;
use ksync::Mutex;
use ktask::current;

use super::{AsThread, CurrentThread, Thread};
use crate::{LiveAddressSpace, Process, Tid};

impl Deref for CurrentThread {
    type Target = Thread;

    fn deref(&self) -> &Self::Target {
        self.0.as_thread()
    }
}

impl CurrentThread {
    /// Returns the current task's subjective credentials.
    pub fn cred(&self) -> Arc<Cred> {
        self.subjective_cred()
    }

    /// Returns an uncommitted copy of the current subjective credentials.
    pub fn prepare_creds(&self) -> Cred {
        self.cred().prepare()
    }

    /// Commits a prepared credential to the current task.
    ///
    /// # Panics
    ///
    /// Panics if objective and subjective pointers differ because credentials
    /// are overridden when commit is attempted.
    pub fn commit_creds(&self, cred: Cred) {
        self.commit_cred(cred);
    }
}

/// Returns a handle to the current task for user-thread access.
///
/// The handle itself is not validated here. Dereferencing it requires an
/// installed Thread runtime and panics for a kernel task.
pub fn current_user_thread() -> CurrentThread {
    CurrentThread(current().clone())
}

/// Returns the current user-visible thread identifier.
pub fn current_user_tid() -> Tid {
    current_user_thread().tid()
}

/// Returns the current filesystem context for shared current-path helpers.
///
/// # Panics
///
/// Panics if a current user thread has already detached its filesystem
/// context. A kernel task instead uses the initialized global filesystem context.
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
pub fn current_user_process_address_space() -> LiveAddressSpace {
    current_user_process()
        .address_space()
        .expect("current user thread must still expose process address space")
}

/// Returns the current process's bound MM identity without taking the MM mutex.
///
/// The process binding lock selects the identity. Each MM has an immutable
/// identity, but successful exec replaces the binding and its returned identity.
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

/// Current-task descriptor access, applying the process's live descriptor limit.
/// This view owns only a task reference, not a second files binding.
pub struct CurrentFiles(CurrentThread);

impl Deref for CurrentFiles {
    type Target = kresources::TaskFiles;

    fn deref(&self) -> &Self::Target {
        self.0.files()
    }
}

impl CurrentFiles {
    /// Returns the current process's descriptor soft limit.
    pub fn max_nofile(&self) -> u64 {
        self.0.process_runtime().resources().max_nofile()
    }

    /// Installs an open file using the current process's descriptor soft limit.
    ///
    /// # Errors
    /// Forwards table detachment and descriptor allocation errors.
    pub fn add_file(&self, file: Arc<kvfs::VfsFile>, cloexec: bool) -> kerrno::KResult<i32> {
        self.0.files().add_file(self.max_nofile(), file, cloexec)
    }

    /// Duplicates a descriptor at/above `minimum` using the live process limit.
    ///
    /// # Errors
    /// Forwards source lookup, minimum, detachment and allocation errors.
    pub fn duplicate_file_from(
        &self,
        fd: i32,
        minimum: i32,
        cloexec: bool,
    ) -> kerrno::KResult<i32> {
        self.0
            .files()
            .duplicate_file_from(self.max_nofile(), fd, minimum, cloexec)
    }

    /// Duplicates a descriptor using the current process's descriptor soft limit.
    ///
    /// # Errors
    /// Forwards source lookup, table detachment and allocation errors.
    pub fn duplicate_file(&self, fd: i32, cloexec: bool) -> kerrno::KResult<i32> {
        self.0
            .files()
            .duplicate_file(self.max_nofile(), fd, cloexec)
    }
}

/// Returns descriptor access for the current user task.
///
/// # Panics
/// Dereferencing the view outside a user task panics.
pub fn current_files() -> CurrentFiles {
    CurrentFiles(current_user_thread())
}
