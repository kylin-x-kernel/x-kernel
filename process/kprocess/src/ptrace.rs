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

#[cfg(unittest)]
mod tests {
    use kcred::Cred;
    use kerrno::KError;
    use unittest::{assert, assert_eq, def_test};

    use super::check_cross_process_read_real_creds_access;

    #[def_test]
    fn matching_real_credentials_allow_cross_process_read() {
        let caller = Cred::new(1000, 100);
        let target = Cred::new(1000, 100);

        assert!(check_cross_process_read_real_creds_access(&caller, &target).is_ok());
    }

    #[def_test]
    fn mismatched_real_credentials_deny_cross_process_read() {
        let caller = Cred::new(1000, 100);
        let target = Cred::new(2000, 200);

        assert_eq!(
            check_cross_process_read_real_creds_access(&caller, &target),
            Err(KError::OperationNotPermitted)
        );
    }

    #[def_test]
    fn privileged_credentials_allow_cross_process_read() {
        assert!(
            check_cross_process_read_real_creds_access(&Cred::root(), &Cred::new(1000, 100))
                .is_ok()
        );
    }
}
