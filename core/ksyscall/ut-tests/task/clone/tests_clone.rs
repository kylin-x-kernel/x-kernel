// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_validate_rejects_newns_and_fs() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWNS | CloneFlags::FS);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newipc_and_sysvsem() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWIPC | CloneFlags::SYSVSEM);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newpid_and_thread() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID | CloneFlags::THREAD);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newpid_and_parent() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID | CloneFlags::PARENT);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_accepts_newpid() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID);
    assert!(req.validate_namespace_flags().is_ok());
}

#[def_test]
fn test_validate_rejects_unimplemented_namespaces_with_enosys() {
    for flags in [CloneFlags::NEWNET, CloneFlags::NEWUSER] {
        let mut req = CloneRequest::new();
        req.set_flags(flags);
        let err = req.validate_namespace_flags().unwrap_err();
        // ENOSYS is the expected errno for unimplemented namespaces.
        assert_eq!(
            LinuxError::from(err),
            LinuxError::ENOSYS,
            "expected ENOSYS for {flags:?}"
        );
    }
}

#[def_test]
fn test_validate_rejects_newcgroup_without_capability_model() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWCGROUP);
    let err = req.validate_namespace_flags().unwrap_err();
    assert_eq!(LinuxError::from(err), LinuxError::ENOSYS);
}

#[def_test]
fn test_validate_rejects_newtime_with_enosys() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWTIME);
    let err = req.validate_namespace_flags().unwrap_err();
    assert_eq!(
        LinuxError::from(err),
        LinuxError::ENOSYS,
        "expected ENOSYS for NEWTIME"
    );
}

#[def_test]
fn test_extract_namespace_flags_includes_newtime() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWNS | CloneFlags::NEWUTS | CloneFlags::NEWTIME);
    let ns = req.extract_namespace_flags();
    assert!(ns.contains(NamespaceFlags::NEWNS));
    assert!(ns.contains(NamespaceFlags::NEWUTS));
    assert!(ns.contains(NamespaceFlags::NEWTIME));
}

#[def_test]
fn test_parse_exit_signal_rejects_invalid_signal_number() {
    assert!(CloneRequest::parse_exit_signal(0).unwrap().is_none());
    assert_eq!(
        CloneRequest::parse_exit_signal(SIGCHLD as u64).unwrap(),
        Some(Signo::SIGCHLD)
    );
    assert!(CloneRequest::parse_exit_signal(65).is_err());
    assert!(CloneRequest::parse_exit_signal(1 << 32).is_err());
}

#[def_test]
fn test_clone3_validation_rejects_exit_signal_with_shared_parent() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::PARENT)
        .set_exit_signal(SIGCHLD as u64)
        .reject_exit_signal_with_shared_parent();

    assert!(req.validate_exit_signal_policy().is_err());
}
