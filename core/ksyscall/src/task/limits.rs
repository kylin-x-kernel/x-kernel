// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process resource-limit syscalls.

use kerrno::{KError, KResult};
use kprocess::Pid;
use linux_raw_sys::general::{RLIM_NLIMITS, rlimit64};
use posix_types::{UserConstPtr, UserPtr};

/// Gets and/or sets resource limits for a process.
pub(crate) fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: UserConstPtr<rlimit64>,
    old_limit: UserPtr<rlimit64>,
) -> KResult<isize> {
    // Linux copies input before PID lookup and output: aliased buffers must
    // preserve the requested update, and input EFAULT precedes ESRCH.
    let new_limit = new_limit
        .check_non_null()
        .map(|value| value.read_vm())
        .transpose()?;
    let process = kprocess::resource_limits::target_process(pid)?;
    let resources = process.resources()?;
    let (current, max) = resources.rlimit_values(resource)?;

    // A rejected update must not overwrite the caller's old-limit buffer.
    if let Some(new_limit) = new_limit {
        resources.set_rlimit_values(resource, new_limit.rlim_cur, new_limit.rlim_max)?;
    }
    if let Some(old_limit) = old_limit.check_non_null() {
        old_limit.write_vm(rlimit64 {
            rlim_cur: current,
            rlim_max: max,
        })?;
    }

    Ok(0)
}

/// Returns the current resource limits of the calling process.
pub(crate) fn sys_getrlimit(resource: u32, old_limit: UserPtr<rlimit64>) -> KResult<isize> {
    // Legacy getrlimit validates the resource before copying its output.
    if resource >= RLIM_NLIMITS {
        return Err(KError::InvalidInput);
    }
    if old_limit.check_non_null().is_none() {
        return Err(KError::BadAddress);
    }
    sys_prlimit64(0, resource, UserConstPtr::from(0usize), old_limit)
}

/// Updates the resource limits of the calling process.
pub(crate) fn sys_setrlimit(resource: u32, new_limit: UserConstPtr<rlimit64>) -> KResult<isize> {
    if new_limit.check_non_null().is_none() {
        return Err(KError::BadAddress);
    }
    sys_prlimit64(0, resource, new_limit, UserPtr::from(0usize))
}

#[unittest::mod_test]
mod tests {
    use kerrno::KError;
    use linux_raw_sys::general::{RLIM_NLIMITS, RLIMIT_NOFILE, rlimit64};
    use posix_types::{UserConstPtr, UserPtr};
    use unittest::{TestResult, assert, assert_eq, def_test};
    use unittest_support::TestUserValue;

    #[def_test]
    fn legacy_rlimit_syscalls_require_non_null_buffers() {
        assert_eq!(
            super::sys_getrlimit(0, UserPtr::default()),
            Err(KError::BadAddress)
        );
        assert_eq!(
            super::sys_setrlimit(0, UserConstPtr::default()),
            Err(KError::BadAddress)
        );
        // Linux's legacy getter and setter deliberately have different errno priority.
        assert_eq!(
            super::sys_getrlimit(RLIM_NLIMITS, UserPtr::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            super::sys_setrlimit(RLIM_NLIMITS, UserConstPtr::default()),
            Err(KError::BadAddress)
        );
    }

    #[def_test(user, serial)]
    fn prlimit_aliased_buffer_updates_limit_and_returns_old_pair() -> TestResult {
        let original = TestUserValue::from_value(rlimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        })
        .unwrap();
        assert_eq!(
            super::sys_prlimit64(
                0,
                RLIMIT_NOFILE,
                UserConstPtr::default(),
                UserPtr::from(original.as_user_ptr())
            ),
            Ok(0)
        );
        let old = original.read();
        assert!(old.rlim_cur > 0);
        let requested = old.rlim_cur - 1;
        // Allocate every fixture before changing the process limit.
        let alias = TestUserValue::from_value(rlimit64 {
            rlim_cur: requested,
            rlim_max: old.rlim_max,
        })
        .unwrap();
        let query = TestUserValue::from_value(rlimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        })
        .unwrap();
        let alias_ptr = UserPtr::from(alias.as_user_ptr());
        // Framework assertions return from this closure; restoration still runs.
        let outcome = (|| -> TestResult {
            assert_eq!(
                super::sys_prlimit64(0, RLIMIT_NOFILE, alias_ptr.as_const(), alias_ptr),
                Ok(0)
            );
            let returned = alias.read();
            assert_eq!(returned.rlim_cur, old.rlim_cur);
            assert_eq!(returned.rlim_max, old.rlim_max);
            assert_eq!(
                super::sys_prlimit64(
                    0,
                    RLIMIT_NOFILE,
                    UserConstPtr::default(),
                    UserPtr::from(query.as_user_ptr())
                ),
                Ok(0)
            );
            let applied = query.read();
            assert_eq!(applied.rlim_cur, requested);
            assert_eq!(applied.rlim_max, old.rlim_max);
            TestResult::Ok
        })();
        // The hard limit is unchanged, so restoring the soft limit needs no elevation.
        let restored = super::sys_prlimit64(
            0,
            RLIMIT_NOFILE,
            UserPtr::from(original.as_user_ptr()).as_const(),
            UserPtr::default(),
        );
        assert_eq!(restored, Ok(0));
        outcome
    }

    #[def_test(user, serial)]
    fn prlimit_rejected_update_preserves_aliased_buffer() {
        // cur > max is invalid without depending on privilege or current limits.
        let alias = TestUserValue::from_value(rlimit64 {
            rlim_cur: 2,
            rlim_max: 1,
        })
        .unwrap();
        let ptr = UserPtr::from(alias.as_user_ptr());
        assert_eq!(
            super::sys_prlimit64(0, RLIMIT_NOFILE, ptr.as_const(), ptr),
            Err(KError::InvalidInput)
        );
        let unchanged = alias.read();
        assert_eq!(unchanged.rlim_cur, 2);
        assert_eq!(unchanged.rlim_max, 1);
    }

    #[def_test(user, serial)]
    fn prlimit_input_fault_precedes_pid_and_resource_errors() {
        // This fresh user task has no page-zero mapping. Prove the input faults.
        let invalid = UserConstPtr::<rlimit64>::from(1usize);
        assert!(matches!(
            invalid.read_vm().map_err(KError::from),
            Err(KError::BadAddress)
        ));
        assert_eq!(
            super::sys_prlimit64(
                kprocess::Pid::MAX,
                RLIMIT_NOFILE,
                invalid,
                UserPtr::default()
            ),
            Err(KError::BadAddress)
        );
        assert_eq!(
            super::sys_setrlimit(RLIM_NLIMITS, invalid),
            Err(KError::BadAddress)
        );
        assert_eq!(
            super::sys_prlimit64(0, RLIM_NLIMITS, UserConstPtr::default(), UserPtr::default()),
            Err(KError::InvalidInput)
        );
    }
}
