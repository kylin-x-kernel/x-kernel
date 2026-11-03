// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ptrace-style cross-task access checks.

use alloc::sync::Arc;

use kcred::{Cred, PtraceAccessMode};
use kerrno::{KError, KResult};

use crate::Thread;

/// Checks whether `caller` may inspect `target` using Linux
/// `PTRACE_MODE_READ_REALCREDS` semantics.
///
/// Threads in the same thread group may inspect each other. For a different
/// thread group, the caller's real UID/GID must match all of the target's
/// real, effective, and saved IDs, and the target's permitted capability set
/// must be a subset of the caller's permitted set. Effective `CAP_SYS_PTRACE`
/// overrides both checks for a different thread group.
///
/// # Errors
///
/// Returns [`KError::OperationNotPermitted`] when neither the identity and
/// capability check nor the privileged override permits access.
pub fn check_read_real_creds_access(caller: &Thread, target: &Thread) -> KResult<()> {
    if Arc::ptr_eq(caller.process(), target.process()) {
        return Ok(());
    }

    let caller_cred = caller.real_cred();
    let target_cred = target.real_cred();
    check_cross_process_read_access(&caller_cred, &target_cred, PtraceAccessMode::RealCreds)
}

/// `PTRACE_MODE_READ_FSCREDS` semantics -- what Linux's `/proc/<pid>/fdinfo`
/// permission hook uses. Threads in the same thread group may inspect each
/// other; for a different thread group the caller's filesystem UID/GID must
/// match all of the target's real, effective, and saved IDs, and the target's
/// permitted capability set must be a subset of the caller's effective set.
/// Effective `CAP_SYS_PTRACE` overrides both checks for a different thread
/// group.
///
/// # Errors
///
/// Returns [`KError::OperationNotPermitted`] when neither the identity and
/// capability check nor the privileged override permits access.
pub fn check_read_fs_creds_access(caller: &Thread, target: &Thread) -> KResult<()> {
    if Arc::ptr_eq(caller.process(), target.process()) {
        return Ok(());
    }

    let caller_cred = caller.real_cred();
    let target_cred = target.real_cred();
    check_cross_process_read_access(&caller_cred, &target_cred, PtraceAccessMode::FsCreds)
}

fn check_cross_process_read_access(
    caller: &Cred,
    target: &Cred,
    mode: PtraceAccessMode,
) -> KResult<()> {
    if caller.can_read_ptrace(target, mode) {
        Ok(())
    } else {
        Err(KError::OperationNotPermitted)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/ptrace.rs");
