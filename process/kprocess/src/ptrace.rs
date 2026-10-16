// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ptrace-style cross-task access checks.

use alloc::sync::Arc;

use kcred::Cred;
use kerrno::{KError, KResult};

use crate::Thread;

/// Checks whether `caller` may inspect `target` using Linux
/// `PTRACE_MODE_READ_REALCREDS` semantics.
///
/// Threads in the same thread group may inspect each other. For a different
/// thread group, the caller's real UID/GID must match all of the target's
/// real, effective, and saved IDs. Until capability sets are modeled, an
/// effective UID of 0 stands in for `CAP_SYS_PTRACE`.
///
/// # Errors
///
/// Returns [`KError::OperationNotPermitted`] when neither the identity match
/// nor the privileged override permits access.
pub fn check_read_real_creds_access(caller: &Thread, target: &Thread) -> KResult<()> {
    if Arc::ptr_eq(caller.process(), target.process()) {
        return Ok(());
    }

    let caller_cred = caller.real_cred();
    let target_cred = target.real_cred();
    check_cross_process_read_real_creds_access(&caller_cred, &target_cred)
}

fn check_cross_process_read_real_creds_access(caller: &Cred, target: &Cred) -> KResult<()> {
    if caller.is_privileged() || target.matches_real_credential_ids(caller) {
        Ok(())
    } else {
        Err(KError::OperationNotPermitted)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/ptrace.rs");
