// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{
    CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWTIME,
    CLONE_NEWUSER, CLONE_NEWUTS,
};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn unshare_flag_set_parses_supported_namespace_and_detach_flags() {
    let raw = (CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID) as u64
        | CLONE_FS as u64
        | CLONE_FILES as u64;
    let (namespaces, detach_fs, detach_files) = unshare_flag_set(raw, false).unwrap();
    assert!(namespaces.contains(NamespaceFlags::NEWNS));
    assert!(namespaces.contains(NamespaceFlags::NEWUTS));
    assert!(namespaces.contains(NamespaceFlags::NEWIPC));
    assert!(namespaces.contains(NamespaceFlags::NEWPID));
    assert!(detach_fs && detach_files);
}

#[def_test]
fn unshare_flag_set_rejects_unknown_bits() {
    assert!(unshare_flag_set(1 << 0, false).is_err());
    assert!(unshare_flag_set(u64::MAX, false).is_err());
}

#[def_test]
fn unshare_flag_set_keeps_explicitly_unsupported_namespace_flags() {
    // NEWNET/NEWUSER/NEWCGROUP/NEWTIME pass the flag-set gate and are
    // rejected with EOPNOTSUPP in the prepare phase instead.
    for raw in [
        CLONE_NEWNET as u64,
        CLONE_NEWUSER as u64,
        CLONE_NEWCGROUP as u64,
        CLONE_NEWTIME as u64,
    ] {
        let (namespaces, ..) = unshare_flag_set(raw, false).unwrap();
        assert_eq!(namespaces.bits(), raw);
    }
}

#[def_test]
fn unshare_flag_set_newns_implies_clone_fs() {
    // Linux's ksys_unshare adds CLONE_FS itself when CLONE_NEWNS is given;
    // callers never need to spell it out.
    let (namespaces, detach_fs, detach_files) =
        unshare_flag_set(CLONE_NEWNS as u64, true).unwrap();
    assert!(namespaces.contains(NamespaceFlags::NEWNS));
    assert!(detach_fs);
    assert!(!detach_files);
}

#[def_test]
fn pid_ns_reachable_accepts_self_and_descendants() {
    let root = Arc::new(PidNamespace::new_root());
    let child = PidNamespace::new_child(&root);
    let grandchild = PidNamespace::new_child(&child);
    assert!(pid_ns_reachable(&root, &root));
    assert!(pid_ns_reachable(&root, &child));
    assert!(pid_ns_reachable(&root, &grandchild));
    assert!(pid_ns_reachable(&child, &grandchild));
}

#[def_test]
fn pid_ns_reachable_rejects_ancestors_and_unrelated_trees() {
    let root = Arc::new(PidNamespace::new_root());
    let child = PidNamespace::new_child(&root);
    // Entering an ancestor escapes the caller's own namespace.
    assert!(!pid_ns_reachable(&child, &root));
    // Two independent roots are sibling trees: never reachable from one
    // another, at any nesting depth.
    let other = Arc::new(PidNamespace::new_root());
    let other_child = PidNamespace::new_child(&other);
    assert!(!pid_ns_reachable(&child, &other));
    assert!(!pid_ns_reachable(&child, &other_child));
    assert!(!pid_ns_reachable(&other_child, &child));
}

#[def_test]
fn unshare_flag_set_accepts_thread_state_bits_only_for_single_thread() {
    let thread_bits = (CLONE_VM | CLONE_SIGHAND | CLONE_THREAD) as u64;
    // A single-threaded caller has nothing to detach: the bits are no-ops.
    let (namespaces, detach_fs, detach_files) =
        unshare_flag_set(thread_bits | CLONE_NEWNS as u64, false).unwrap();
    assert!(namespaces.contains(NamespaceFlags::NEWNS));
    assert!(detach_fs && !detach_files);
    // A multithreaded caller is rejected with EINVAL, as in Linux.
    assert!(unshare_flag_set(thread_bits, true).is_err());
}
