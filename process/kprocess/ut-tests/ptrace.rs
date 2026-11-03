// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::{CAP_SYS_NICE, CAP_SYS_PTRACE, CapSet, CapSets, Cred, PtraceAccessMode};
use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

use super::check_cross_process_read_access;

fn root_with_capabilities(effective: CapSet, permitted: CapSet) -> Cred {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(effective, permitted, CapSet::EMPTY))
        .unwrap();
    credentials
}

#[def_test]
fn matching_real_credentials_allow_cross_process_read() {
    let caller = Cred::new(1000, 100);
    let target = Cred::new(1000, 100);

    assert!(
        check_cross_process_read_access(&caller, &target, PtraceAccessMode::RealCreds).is_ok()
    );
}

#[def_test]
fn mismatched_real_credentials_deny_cross_process_read() {
    let caller = Cred::new(1000, 100);
    let target = Cred::new(2000, 200);

    assert_eq!(
        check_cross_process_read_access(&caller, &target, PtraceAccessMode::RealCreds),
        Err(KError::OperationNotPermitted)
    );
}

#[def_test]
fn privileged_credentials_allow_cross_process_read() {
    assert!(
        check_cross_process_read_access(&Cred::root(), &Cred::new(1000, 100), PtraceAccessMode::RealCreds)
            .is_ok()
    );
}

#[def_test]
fn capability_empty_caller_cannot_read_privileged_target() {
    let caller = root_with_capabilities(CapSet::EMPTY, CapSet::EMPTY);
    let target = Cred::root();

    assert_eq!(
        check_cross_process_read_access(&caller, &target, PtraceAccessMode::RealCreds),
        Err(KError::OperationNotPermitted)
    );
}

#[def_test]
fn filesystem_credentials_check_rejects_permitted_but_ineffective_capability() {
    let caller = root_with_capabilities(CapSet::EMPTY, CapSet::FULL);
    let target = root_with_capabilities(CapSet::EMPTY, CapSet::from_bits(1 << CAP_SYS_NICE));

    assert_eq!(
        check_cross_process_read_access(&caller, &target, PtraceAccessMode::FsCreds),
        Err(KError::OperationNotPermitted)
    );
}

#[def_test]
fn effective_cap_sys_ptrace_allows_cross_process_read() {
    let caller = root_with_capabilities(
        CapSet::from_bits(1 << CAP_SYS_PTRACE),
        CapSet::from_bits(1 << CAP_SYS_PTRACE),
    );
    let target = Cred::new(2000, 200);

    assert!(
        check_cross_process_read_access(&caller, &target, PtraceAccessMode::RealCreds).is_ok()
    );
}
