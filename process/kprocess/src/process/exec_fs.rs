// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-context admission for an exec transition.

use alloc::sync::Arc;

use fs_context::{ExecAdmission, FsStruct};
use kerrno::KResult;
use ksync::Mutex;

use super::Process;

/// Holds the filesystem-context admission for an exec transition.
///
/// An exclusive admission sets `in_exec`; dropping the guard clears it, so
/// every error path automatically reopens the context for `CLONE_FS` adoption.
/// A shared admission changes nothing and only records that the exec is unsafe.
#[must_use = "the exec transition stays active until the guard is dropped"]
pub struct ExecFsGuard {
    fs_context: Arc<Mutex<FsStruct>>,
    shared: bool,
}

impl ExecFsGuard {
    /// Returns whether another process owned the context during exec admission.
    ///
    /// A shared context corresponds to Linux `LSM_UNSAFE_SHARE`; the exec must
    /// not gain privileges even when the caller is otherwise privileged.
    pub fn is_shared_with_other_process(&self) -> bool {
        self.shared
    }
}

impl Drop for ExecFsGuard {
    fn drop(&mut self) {
        if !self.shared {
            self.fs_context.lock().end_exec();
        }
    }
}

impl Process {
    /// Starts an exec transition on this process's filesystem context.
    ///
    /// The sharing test and admission happen atomically under the
    /// filesystem-context lock. An exclusive admission sets `in_exec`, so a
    /// concurrent `CLONE_FS` fork either adds an owner before the admission
    /// (making the exec shared) or fails with [`kerrno::KError::WouldBlock`]
    /// until the returned guard is dropped. A shared admission records the
    /// unsafe state without blocking further adoption, matching Linux.
    ///
    /// # Errors
    ///
    /// Returns [`kerrno::KError::NoSuchProcess`] when this process has already
    /// released its filesystem context.
    pub fn begin_exec(&self) -> KResult<ExecFsGuard> {
        let fs_context = self.fs_context()?;
        let shared = match fs_context.lock().begin_exec() {
            ExecAdmission::Exclusive => false,
            ExecAdmission::Shared => true,
        };
        Ok(ExecFsGuard { fs_context, shared })
    }
}
