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
fn prctl_securebits_set_get_and_keep_caps_are_consistent() {
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0));

    // NO_SETUID_FIXUP | NO_CAP_AMBIENT_RAISE.
    assert_eq!(sys_prctl(PR_SET_SECUREBITS, 0x44, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0x44));

    // PR_SET_KEEPCAPS must be observable through PR_GET_SECUREBITS too.
    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(1));
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0x54));

    assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0x44));

    // Modern exec-check settings are accepted even though this kernel does not
    // implement AT_EXECVE_CHECK.
    assert_eq!(sys_prctl(PR_SET_SECUREBITS, 0x544, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0x544));
    assert_eq!(sys_prctl(PR_SET_SECUREBITS, 0x44, 0, 0, 0), Ok(0));

    assert_eq!(
        sys_prctl(PR_SET_SECUREBITS, 1 << 40, 0, 0, 0),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0x44));

    assert_eq!(sys_prctl(PR_SET_SECUREBITS, 0, 0, 0, 0), Ok(0));
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
fn prctl_capbset_drop_removes_the_capability_from_the_bounding_set() {
    // CAP_NET_RAW = 13 is in the initial bounding set.
    assert_eq!(sys_prctl(PR_CAPBSET_READ, 13, 0, 0, 0), Ok(1));
    assert_eq!(sys_prctl(PR_CAPBSET_DROP, 13, 0, 0, 0), Ok(0));
    // Dropping is one-way: the capability no longer reads back as present.
    assert_eq!(sys_prctl(PR_CAPBSET_READ, 13, 0, 0, 0), Ok(0));
    // Re-dropping an already-dropped capability is a successful no-op.
    assert_eq!(sys_prctl(PR_CAPBSET_DROP, 13, 0, 0, 0), Ok(0));
    assert_eq!(sys_prctl(PR_CAPBSET_READ, 13, 0, 0, 0), Ok(0));
    // Dropping one capability leaves the others untouched.
    assert_eq!(sys_prctl(PR_CAPBSET_READ, 12, 0, 0, 0), Ok(1));
}

#[def_test(user, serial)]
fn prctl_capbset_drop_rejects_unknown_capability_with_einval() {
    assert_eq!(
        sys_prctl(PR_CAPBSET_DROP, 9999, 0, 0, 0),
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

mod capabilities {
    use kcred::{CAP_CHOWN, CAP_FOWNER, CAP_SETPCAP, CAP_SYS_ADMIN};
    use kerrno::KError;
    use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};
    use linux_raw_sys::prctl::{
        PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, PR_CAP_AMBIENT_IS_SET, PR_CAP_AMBIENT_LOWER,
        PR_CAP_AMBIENT_RAISE,
    };
    use posix_types::{UserPtr, task::CapUserData};
    use unittest::{assert_eq, def_test};
    use unittest_support::TestUserValue;

    use super::super::{
        CAPABILITY_VERSION_1, CAPABILITY_VERSION_2, CAPABILITY_VERSION_3, capset_targets_caller,
        sys_capget, sys_capset, sys_prctl,
    };

    const FULL_LOW: u32 = u32::MAX;
    const FULL_HIGH: u32 = (1 << (kcred::CAP_LAST_CAP + 1 - 32)) - 1;

    fn bit(capability: u32) -> u32 {
        1 << capability
    }

    fn header(version: u32, pid: i32) -> TestUserValue<__user_cap_header_struct> {
        TestUserValue::from_value(__user_cap_header_struct { version, pid }).unwrap()
    }

    fn data_word(
        effective: u32,
        permitted: u32,
        inheritable: u32,
    ) -> __user_cap_data_struct {
        __user_cap_data_struct {
            effective,
            permitted,
            inheritable,
        }
    }

    fn caps(
        low: __user_cap_data_struct,
        high: __user_cap_data_struct,
    ) -> TestUserValue<CapUserData> {
        TestUserValue::from_value(CapUserData { low, high }).unwrap()
    }

    fn capget(header: &TestUserValue<__user_cap_header_struct>) -> CapUserData {
        let data = TestUserValue::<CapUserData>::new().unwrap();
        core::assert_eq!(
            sys_capget(
                UserPtr::from(header.as_user_ptr()),
                UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>())
            ),
            Ok(0)
        );
        data.read()
    }

    fn capset(
        header: &TestUserValue<__user_cap_header_struct>,
        data: &TestUserValue<CapUserData>,
    ) -> Result<isize, KError> {
        sys_capset(
            UserPtr::from(header.as_user_ptr()),
            UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>()),
        )
    }

    fn bits(data: &CapUserData) -> (u32, u32, u32, u32, u32, u32) {
        (
            data.low.effective,
            data.low.permitted,
            data.low.inheritable,
            data.high.effective,
            data.high.permitted,
            data.high.inheritable,
        )
    }

    #[def_test(user, serial)]
    fn capget_version_3_reports_both_words_of_the_root_credential() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let caps = capget(&header);

        assert_eq!(
            bits(&caps),
            (FULL_LOW, FULL_LOW, 0, FULL_HIGH, FULL_HIGH, 0)
        );
    }

    #[def_test(user, serial)]
    fn capget_reports_effective_and_permitted_independently() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let requested = caps(
            data_word(bit(CAP_CHOWN), bit(CAP_CHOWN) | bit(CAP_FOWNER), 0),
            data_word(0, 0, 0),
        );

        assert_eq!(capset(&header, &requested), Ok(0));
        assert_eq!(
            bits(&capget(&header)),
            (
                bit(CAP_CHOWN),
                bit(CAP_CHOWN) | bit(CAP_FOWNER),
                0,
                0,
                0,
                0,
            )
        );
    }

    #[def_test(user, serial)]
    fn capget_version_2_uses_the_two_word_layout() {
        let header = header(CAPABILITY_VERSION_2, 0);
        assert_eq!(
            bits(&capget(&header)),
            (FULL_LOW, FULL_LOW, 0, FULL_HIGH, FULL_HIGH, 0)
        );
    }

    #[def_test(user, serial)]
    fn capget_version_1_writes_only_the_low_word() {
        // Linux copies exactly one `__user_cap_data_struct` for ABI v1. The
        // high word must remain untouched even though the carrier has room.
        let header = header(CAPABILITY_VERSION_1, 0);
        let data = caps(
            data_word(0xaaaa_aaaa, 0xbbbb_bbbb, 0xcccc_cccc),
            data_word(1, 2, 3),
        );
        assert_eq!(
            sys_capget(
                UserPtr::from(header.as_user_ptr()),
                UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>())
            ),
            Ok(0)
        );
        assert_eq!(
            bits(&data.read()),
            (FULL_LOW, FULL_LOW, 0, 1, 2, 3)
        );
    }

    #[def_test(user, serial)]
    fn capset_version_1_ignores_the_high_word() {
        let header_v1 = header(CAPABILITY_VERSION_1, 0);
        // A v1 caller cannot address capabilities above 31. The sentinel high
        // word must not leak into the credential, even though it is readable.
        let requested = caps(
            data_word(FULL_LOW, FULL_LOW, 0),
            data_word(FULL_HIGH, FULL_HIGH, FULL_HIGH),
        );
        assert_eq!(capset(&header_v1, &requested), Ok(0));

        assert_eq!(
            bits(&capget(&header(CAPABILITY_VERSION_3, 0))),
            (FULL_LOW, FULL_LOW, 0, 0, 0, 0)
        );
    }

    #[def_test(user, serial)]
    fn capset_version_2_accepts_upper_capability_bits() {
        let header = header(CAPABILITY_VERSION_2, 0);
        let requested = caps(
            data_word(FULL_LOW, FULL_LOW, 0),
            data_word(FULL_HIGH, FULL_HIGH, FULL_HIGH),
        );
        assert_eq!(capset(&header, &requested), Ok(0));

        assert_eq!(
            bits(&capget(&header)),
            (FULL_LOW, FULL_LOW, 0, FULL_HIGH, FULL_HIGH, FULL_HIGH)
        );
    }

    #[def_test(user, serial)]
    fn capset_masks_capability_bits_above_cap_last_cap() {
        let header = header(CAPABILITY_VERSION_3, 0);
        // CAP_LAST_CAP is 40, so bit 9 of the high word names capability 41.
        let unknown = 1u32 << 9;
        let requested = caps(
            data_word(FULL_LOW, FULL_LOW, 0),
            data_word(
                FULL_HIGH | unknown,
                FULL_HIGH | unknown,
                unknown,
            ),
        );
        assert_eq!(capset(&header, &requested), Ok(0));

        // Linux `mk_kernel_cap()` drops unknown bits instead of installing
        // them or rejecting an otherwise valid transition.
        assert_eq!(
            bits(&capget(&header)),
            (FULL_LOW, FULL_LOW, 0, FULL_HIGH, FULL_HIGH, 0)
        );
    }

    #[def_test(user, serial)]
    fn capget_rejects_negative_pids_with_einval() {
        for pid in [-1, -1000, i32::MIN] {
            let header = header(CAPABILITY_VERSION_3, pid);
            let data = caps(data_word(0xaaaa_aaaa, 0, 0), data_word(0, 0, 0));
            assert_eq!(
                sys_capget(
                    UserPtr::from(header.as_user_ptr()),
                    UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>())
                ),
                Err(KError::InvalidInput)
            );
            assert_eq!(bits(&data.read()), (0xaaaa_aaaa, 0, 0, 0, 0, 0));
        }
    }

    #[def_test(user, serial)]
    fn capget_reports_esrch_for_a_missing_positive_pid() {
        let header = header(CAPABILITY_VERSION_3, i32::MAX);
        let data = caps(data_word(0xaaaa_aaaa, 0, 0), data_word(0, 0, 0));
        assert_eq!(
            sys_capget(
                UserPtr::from(header.as_user_ptr()),
                UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>())
            ),
            Err(KError::NoSuchProcess)
        );
        assert_eq!(bits(&data.read()), (0xaaaa_aaaa, 0, 0, 0, 0, 0));
    }

    #[def_test(user, serial)]
    fn capget_accepts_the_callers_thread_id() {
        let pid = kprocess::current_user_tid() as i32;
        let header = header(CAPABILITY_VERSION_3, pid);
        assert_eq!(
            bits(&capget(&header)),
            (FULL_LOW, FULL_LOW, 0, FULL_HIGH, FULL_HIGH, 0)
        );
    }

    #[def_test(user, serial)]
    fn capget_null_data_is_a_version_probe_that_skips_the_pid() {
        // A negative PID would fail with EINVAL if the legacy probe read it.
        let valid = header(CAPABILITY_VERSION_3, i32::MIN);
        let null = UserPtr::<__user_cap_data_struct>::from(0);
        assert_eq!(
            sys_capget(UserPtr::from(valid.as_user_ptr()), null),
            Ok(0)
        );

        let unknown = header(0xdead_beef, 0);
        assert_eq!(
            sys_capget(UserPtr::from(unknown.as_user_ptr()), null),
            Ok(0)
        );
        assert_eq!(unknown.read().version, CAPABILITY_VERSION_3);
    }

    #[def_test(user, serial)]
    fn capget_rewrites_an_unknown_version_without_touching_data() {
        let header = header(0xdead_beef, 0);
        let data = caps(data_word(0xaaaa_aaaa, 0xbbbb_bbbb, 0xcccc_cccc), data_word(1, 2, 3));

        assert_eq!(
            sys_capget(
                UserPtr::from(header.as_user_ptr()),
                UserPtr::from(data.as_user_ptr().cast::<__user_cap_data_struct>())
            ),
            Err(KError::InvalidInput)
        );
        assert_eq!(header.read().version, CAPABILITY_VERSION_3);
        assert_eq!(
            bits(&data.read()),
            (0xaaaa_aaaa, 0xbbbb_bbbb, 0xcccc_cccc, 1, 2, 3)
        );
    }

    #[def_test(user, serial)]
    fn capset_rejects_unknown_version_before_reading_data() {
        let header = header(0, 0);
        let bogus = UserPtr::<__user_cap_data_struct>::from(0);

        assert_eq!(
            sys_capset(UserPtr::from(header.as_user_ptr()), bogus),
            Err(KError::InvalidInput)
        );
        assert_eq!(header.read().version, CAPABILITY_VERSION_3);
    }

    #[def_test(user, serial)]
    fn capset_rejects_other_targets_before_reading_data() {
        for pid in [-1, -1000, i32::MIN, i32::MAX] {
            let header = header(CAPABILITY_VERSION_3, pid);
            let bogus = UserPtr::<__user_cap_data_struct>::from(0);
            assert_eq!(
                sys_capset(UserPtr::from(header.as_user_ptr()), bogus),
                Err(KError::OperationNotPermitted)
            );
        }
    }

    #[def_test(user, serial)]
    fn capset_rejects_null_data_with_efault() {
        let header = header(CAPABILITY_VERSION_3, 0);
        assert_eq!(
            sys_capset(
                UserPtr::from(header.as_user_ptr()),
                UserPtr::<__user_cap_data_struct>::from(0)
            ),
            Err(KError::BadAddress)
        );
    }

    #[def_test(user, serial)]
    fn capset_accepts_current_namespace_thread_id() {
        let pid = kprocess::current_user_thread().task_number().active_nr() as i32;
        let header = header(CAPABILITY_VERSION_3, pid);
        let requested = caps(
            data_word(bit(CAP_CHOWN), bit(CAP_CHOWN), 0),
            data_word(0, 0, 0),
        );

        assert_eq!(capset(&header, &requested), Ok(0));
        assert_eq!(bits(&capget(&header)), (1, 1, 0, 0, 0, 0));
    }

    #[def_test(user, serial)]
    fn capset_target_accepts_both_of_the_callers_reported_tids() {
        // A nested PID-namespace leader reports 1 through getpid(2) but its
        // root-allocated thread identity through gettid(2). Both name the
        // caller, while neither another active number nor another root TID
        // may be accepted.
        assert!(capset_targets_caller(0, 1, 4242));
        assert!(capset_targets_caller(1, 1, 4242));
        assert!(capset_targets_caller(4242, 1, 4242));
        for pid in [-1, i32::MIN, 2, 4241, 4243, i32::MAX] {
            assert!(!capset_targets_caller(pid, 1, 4242));
        }
    }

    #[def_test(user, serial)]
    fn capset_enforces_subset_rules_atomically() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let initial = caps(
            data_word(bit(CAP_CHOWN), bit(CAP_CHOWN), 0),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &initial), Ok(0));
        let before = bits(&capget(&header));

        for rejected in [
            caps(
                data_word(bit(CAP_SYS_ADMIN), bit(CAP_CHOWN), 0),
                data_word(0, 0, 0),
            ),
            caps(
                data_word(bit(CAP_CHOWN), bit(CAP_CHOWN), bit(CAP_SYS_ADMIN)),
                data_word(0, 0, 0),
            ),
            caps(
                data_word(bit(CAP_CHOWN), bit(CAP_CHOWN) | bit(CAP_SYS_ADMIN), 0),
                data_word(0, 0, 0),
            ),
        ] {
            assert_eq!(
                capset(&header, &rejected),
                Err(KError::OperationNotPermitted)
            );
            assert_eq!(bits(&capget(&header)), before);
        }
    }

    #[def_test(user, serial)]
    fn capset_effective_setpcap_allows_inheritable_growth() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let requested = caps(
            data_word(bit(CAP_SETPCAP), bit(CAP_SETPCAP), 0),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &requested), Ok(0));

        let grown = caps(
            data_word(
                bit(CAP_SETPCAP),
                bit(CAP_SETPCAP),
                bit(CAP_SYS_ADMIN),
            ),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &grown), Ok(0));
        assert_eq!(
            bits(&capget(&header)),
            (
                bit(CAP_SETPCAP),
                bit(CAP_SETPCAP),
                bit(CAP_SYS_ADMIN),
                0,
                0,
                0,
            )
        );
    }

    #[def_test(user, serial)]
    fn capset_never_raises_permitted_even_with_effective_setpcap() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let requested = caps(
            data_word(bit(CAP_SETPCAP), bit(CAP_SETPCAP), 0),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &requested), Ok(0));
        let before = bits(&capget(&header));

        let raised = caps(
            data_word(
                bit(CAP_SETPCAP),
                bit(CAP_SETPCAP) | bit(CAP_SYS_ADMIN),
                0,
            ),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &raised), Err(KError::OperationNotPermitted));
        assert_eq!(bits(&capget(&header)), before);
    }

    #[def_test(user, serial)]
    fn cap_ambient_raise_lower_and_clear_use_the_committed_credential() {
        let header = header(CAPABILITY_VERSION_3, 0);
        let capability = CAP_CHOWN as usize;
        let raise = || {
            sys_prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_RAISE as usize,
                capability,
                0,
                0,
            )
        };
        let is_set = || {
            sys_prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_IS_SET as usize,
                capability,
                0,
                0,
            )
        };

        // The root credential's inheritable set starts empty.
        assert_eq!(raise(), Err(KError::OperationNotPermitted));
        assert_eq!(is_set(), Ok(0));

        let requested = caps(
            data_word(bit(CAP_CHOWN), bit(CAP_CHOWN), bit(CAP_CHOWN)),
            data_word(0, 0, 0),
        );
        assert_eq!(capset(&header, &requested), Ok(0));
        assert_eq!(raise(), Ok(0));
        assert_eq!(is_set(), Ok(1));
        assert_eq!(raise(), Ok(0));

        assert_eq!(
            sys_prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_LOWER as usize,
                capability,
                0,
                0
            ),
            Ok(0)
        );
        assert_eq!(is_set(), Ok(0));

        assert_eq!(raise(), Ok(0));
        assert_eq!(
            sys_prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_CLEAR_ALL as usize,
                1,
                0,
                0
            ),
            Err(KError::InvalidInput)
        );
        assert_eq!(is_set(), Ok(1));
        assert_eq!(
            sys_prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_CLEAR_ALL as usize,
                0,
                0,
                0
            ),
            Ok(0)
        );
        assert_eq!(is_set(), Ok(0));
    }
}

#[def_test(user, serial)]
fn prctl_get_auxv_copies_the_saved_vector() {
    use unittest_support::TestUserArray;

    // The kernel retains the exec-time vector per process. The unittest
    // fixture is created without an `execve`, so publish a known vector
    // first; that also mirrors Linux's save-at-exec contract.
    let process = kprocess::current_user_process();
    process.set_saved_aux(alloc::vec![6, 4096, 17, 100, 0, 0]);
    let buffer = TestUserArray::<u8, 1024>::new().unwrap();
    let size = sys_prctl(PR_GET_AUXV, buffer.as_user_ptr() as usize, 1024, 0, 0).unwrap();
    // Linux returns the size of the whole saved vector; the exact value is
    // an implementation detail (this kernel stores it densely).
    assert!(size as usize >= 2 * core::mem::size_of::<usize>());
    let bytes = buffer.read();
    // Walk the (type, value) pairs and require the Linux-required entries and
    // the trailing AT_NULL terminator.
    let words = size as usize / core::mem::size_of::<usize>();
    let mut found_pagesz = false;
    let mut found_clktck = false;
    let mut found_null = false;
    let mut index = 0;
    while index + 1 < words {
        let kind = usize::from_ne_bytes(
            bytes[index * size_of::<usize>()..(index + 1) * size_of::<usize>()]
                .try_into()
                .unwrap(),
        );
        let value = usize::from_ne_bytes(
            bytes[(index + 1) * size_of::<usize>()..(index + 2) * size_of::<usize>()]
                .try_into()
                .unwrap(),
        );
        match kind {
            6 => {
                assert_eq!(value, 4096);
                found_pagesz = true;
            }
            17 => {
                assert_eq!(value, 100);
                found_clktck = true;
            }
            0 => {
                found_null = true;
                break;
            }
            _ => {}
        }
        index += 2;
    }
    assert!(found_pagesz);
    assert!(found_clktck);
    assert!(found_null);
}

#[def_test(user, serial)]
fn prctl_get_auxv_rejects_nonzero_arg4_or_arg5() {
    use unittest_support::TestUserArray;

    let buffer = TestUserArray::<u8, 64>::new().unwrap();
    assert_eq!(
        sys_prctl(PR_GET_AUXV, buffer.as_user_ptr() as usize, 64, 1, 0),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        sys_prctl(PR_GET_AUXV, buffer.as_user_ptr() as usize, 64, 0, 1),
        Err(KError::InvalidInput)
    );
}
