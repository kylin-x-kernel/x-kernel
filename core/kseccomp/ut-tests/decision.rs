// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Return-value decoding and decision-precedence tests.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::decision::{
    SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD,
    SECCOMP_RET_LOG, SECCOMP_RET_TRACE, SECCOMP_RET_TRAP, SECCOMP_RET_USER_NOTIF,
};

#[def_test]
fn action_decoding() {
    assert_eq!(Action::from_ret(SECCOMP_RET_ALLOW), Action::Allow);
    assert_eq!(
        Action::from_ret(SECCOMP_RET_KILL_PROCESS),
        Action::KillProcess
    );
    assert_eq!(
        Action::from_ret(SECCOMP_RET_KILL_THREAD),
        Action::KillThread
    );
    assert_eq!(Action::from_ret(SECCOMP_RET_ERRNO | 3), Action::Errno(3));
    assert_eq!(Action::from_ret(SECCOMP_RET_LOG), Action::Log);
    assert_eq!(Action::from_ret(SECCOMP_RET_USER_NOTIF), Action::UserNotif);
    assert_eq!(Action::from_ret(SECCOMP_RET_TRACE), Action::Trace);
}

#[def_test]
fn errno_data_masked() {
    assert_eq!(
        Action::from_ret(SECCOMP_RET_ERRNO | 0xabcd),
        Action::Errno(0xabcd)
    );
}

#[def_test]
fn precedence_order_is_stable() {
    assert!(Action::KillProcess.precedence() < Action::KillThread.precedence());
    assert!(Action::KillThread.precedence() < Action::Errno(0).precedence());
    assert!(Action::Errno(0).precedence() < Action::Log.precedence());
    assert!(Action::Log.precedence() < Action::Allow.precedence());
}

#[def_test]
fn highest_priority_wins() {
    assert_eq!(
        highest_priority([Action::Allow, Action::Errno(5), Action::KillThread]),
        Decision::KillThread
    );
    assert_eq!(
        highest_priority([Action::KillProcess, Action::Errno(5)]),
        Decision::KillProcess
    );
    assert_eq!(
        highest_priority([Action::Allow, Action::Allow]),
        Decision::Allow
    );
    assert_eq!(highest_priority(core::iter::empty()), Decision::Allow);
}

/// `TRAP` and any unrecognised action must fail closed: Linux delivers
/// `SIGSYS` for `TRAP` and terminates the thread for an unknown value, so the
/// decision can never be `Allow`.
#[def_test]
fn trap_and_unknown_actions_fail_closed() {
    assert_eq!(Action::from_ret(SECCOMP_RET_TRAP), Action::Unknown);
    assert_eq!(Action::from_ret(0x1234_0000), Action::Unknown);

    assert_eq!(highest_priority([Action::Unknown]), Decision::Unsupported);
    assert_eq!(
        highest_priority([Action::Allow, Action::Unknown]),
        Decision::Unsupported
    );
    assert_eq!(
        highest_priority([Action::from_ret(SECCOMP_RET_TRAP)]),
        Decision::Unsupported
    );
    // A filtering chain must not allow just because one filter allowed.
    assert_eq!(
        highest_priority([Action::Allow, Action::Errno(1), Action::Unknown]),
        Decision::Unsupported
    );
}

/// A value below the first real action masks to zero, i.e.
/// `SECCOMP_RET_KILL_THREAD`, so it fails closed rather than being ignored.
#[def_test]
fn sub_action_values_decode_to_the_zero_action() {
    assert_eq!(Action::from_ret(31), Action::KillThread);
    assert_eq!(highest_priority([Action::from_ret(31)]), Decision::KillThread);
}