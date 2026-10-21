// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use linux_raw_sys::prctl::*;
use unittest::{assert_eq, def_test};

use super::{SECCOMP_MODE_FILTER, SECCOMP_MODE_STRICT, sys_prctl};

#[def_test(user, serial)]
fn test_prctl_keep_caps_set_get() {
    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(0));

    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(1));

    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
}

#[def_test(user, serial)]
fn test_prctl_keep_caps_rejects_invalid_value() {
    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
    assert_eq!(
        sys_prctl(PR_SET_KEEPCAPS, 2, 0, 0, 0),
        Err(KError::InvalidInput)
    );
    assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
}

/// `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &fprog)` installs through the
/// `seccomp(2)` path, and `PR_GET_SECCOMP` then reports mode 2.
///
/// The program allows everything so the test thread stays usable; strict
/// mode cannot be exercised here because it would kill this thread at its
/// next non-allowlisted syscall (see `kprocess`'s seccomp context tests).
#[def_test(user, serial)]
fn prctl_set_seccomp_installs_a_filter_and_reports_the_mode() {
    use linux_raw_sys::ptrace::{sock_filter, sock_fprog};
    use unittest_support::{TestUserArray, TestUserValue};

    let program = TestUserArray::<sock_filter, 1>::from_array([sock_filter {
        code: 0x06, // BPF_RET | BPF_K
        jt: 0,
        jf: 0,
        k: 0x7fff_0000,
    }])
    .unwrap();
    let fprog = TestUserValue::from_value(sock_fprog {
        len: 1,
        filter: program.as_user_ptr(),
    })
    .unwrap();

    assert_eq!(
        sys_prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            fprog.as_user_ptr() as usize,
            0,
            0
        ),
        Ok(0)
    );
    assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), Ok(2));
}

#[def_test(user, serial)]
fn prctl_set_seccomp_filter_rejects_an_unreadable_program() {
    // A null or bogus `sock_fprog` pointer must fail without installing.
    let mode_before = sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    for filter_addr in [0, 1] {
        assert!(
            sys_prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, filter_addr, 0, 0).is_err(),
            "an unreadable program must be rejected"
        );
    }
    assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), mode_before);
}

#[def_test(user, serial)]
fn prctl_set_seccomp_rejects_invalid_modes() {
    let nnp_before = sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    let mode_before = sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    for mode in [0, 3, 999, u32::MAX as usize, usize::MAX] {
        for filter_addr in [0, 1, usize::MAX] {
            assert_eq!(
                sys_prctl(PR_SET_SECCOMP, mode, filter_addr, 0, 0),
                Err(KError::InvalidInput)
            );
            assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), mode_before);
            assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), nnp_before);
        }
    }
}

#[def_test(user, serial)]
fn prctl_set_seccomp_preserves_unsigned_long_mode_width() {
    // Unlike seccomp(2)'s operation, this argument must not truncate to u32.
    if let Some(high) = 1usize.checked_shl(32) {
        for mode in [high, high | SECCOMP_MODE_STRICT, high | SECCOMP_MODE_FILTER] {
            assert_eq!(
                sys_prctl(PR_SET_SECCOMP, mode, usize::MAX, 0, 0),
                Err(KError::InvalidInput)
            );
        }
    }
}

#[def_test(user, serial)]
fn prctl_get_seccomp_reports_a_valid_mode_without_writing_arg2() {
    // Linux returns the seccomp mode as the prctl return value and never
    // writes user memory.
    assert!(matches!(
        sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0),
        Ok(mode) if (0..=2).contains(&mode)
    ));
}

#[def_test(user, serial)]
fn prctl_get_seccomp_rejects_nonzero_args_with_einval() {
    // Linux requires arg2..arg5 to be zero for PR_GET_SECCOMP.
    assert_eq!(
        sys_prctl(PR_GET_SECCOMP, 1, 0, 0, 0),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prctl_get_dumpable_reports_default() {
    assert_eq!(sys_prctl(PR_GET_DUMPABLE, 0, 0, 0, 0), Ok(1));
}

#[def_test(user, serial)]
fn prctl_get_securebits_reports_unset() {
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0));
}

#[def_test(user, serial)]
fn prctl_get_no_new_privs_reports_unset() {
    assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), Ok(0));
}

#[def_test(user, serial)]
fn prctl_set_no_new_privs_records_the_flag() {
    // Programs that require this prctl to succeed -- notably Chromium's
    // child-process launcher -- abort subprocess creation on failure, so
    // recording the flag must succeed and be observable via PR_GET.
    assert_eq!(sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), Ok(1));
    // Linux rejects a non-zero argument tail with EINVAL.
    assert_eq!(
        sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prctl_set_no_new_privs_rejects_invalid_args() {
    assert_eq!(
        sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_prctl(PR_SET_NO_NEW_PRIVS, 2, 0, 0, 0),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prctl_set_pdeath_sig_validates_signal_range() {
    // Linux `valid_signal`: 0 (clear) and signals 1..=_NSIG (64, including
    // SIGRTMAX) are accepted.
    assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 0, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 9, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 64, 0, 0, 0), Ok(0));
    // Out-of-range signals are rejected with EINVAL.
    assert_eq!(
        sys_prctl(PR_SET_PDEATHSIG, 65, 0, 0, 0),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prctl_get_thp_disable_reports_disabled() {
    assert_eq!(sys_prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0), Ok(1));
}

#[def_test(user, serial)]
fn prctl_get_timerslack_reports_default() {
    assert_eq!(sys_prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0), Ok(50_000));
}

#[def_test(user, serial)]
fn prctl_capbset_read_reports_in_set() {
    assert_eq!(sys_prctl(PR_CAPBSET_READ, 5, 0, 0, 0), Ok(1));
}

#[def_test(user, serial)]
fn prctl_capbset_read_rejects_unknown_capability_with_einval() {
    assert_eq!(
        sys_prctl(PR_CAPBSET_READ, 9999, 0, 0, 0),
        Err(KError::InvalidInput)
    );
}

#[def_test(user, serial)]
fn prctl_cap_ambient_is_set_reports_unset() {
    assert_eq!(
        sys_prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET as usize, 0, 0, 0),
        Ok(0)
    );
}

#[def_test(user, serial)]
fn prctl_cap_ambient_rejects_unknown_subcommand_with_einval() {
    assert_eq!(
        sys_prctl(PR_CAP_AMBIENT, 0xFFFF, 0, 0, 0),
        Err(KError::InvalidInput)
    );
}
