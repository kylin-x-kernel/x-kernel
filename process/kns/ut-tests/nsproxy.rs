// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kcred::initial_user_namespace;
use kvfs::{DirMapping, MntNamespace, SimpleDir, SimpleFs};
use unittest::def_test;

use super::*;

static TEST_FILE_SYSTEM_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("kns-test");

fn make_mnt_namespace() -> Arc<MntNamespace> {
    let root_fs = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        root.add_dir(
            "mnt",
            SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    MntNamespace::new_root(&root_fs, initial_user_namespace())
}

fn make_nsproxy() -> Arc<NsProxy> {
    NsProxy::new_initial_with_mnt_ns(make_mnt_namespace())
}

fn make_fs(nsproxy: &NsProxy) -> FsStruct {
    FsStruct::new(nsproxy.mnt_ns().visible_root_path())
}

fn clone_with_private_fs(parent: &NsProxy, flags: NamespaceFlags) -> Arc<NsProxy> {
    let mut fs = make_fs(parent);
    parent
        .clone_for_child(flags, NamespaceFsContext::Private(&mut fs))
        .unwrap()
}

#[def_test]
fn test_nsproxy_initial_creation() {
    let first = make_nsproxy();
    let second = make_nsproxy();
    assert_eq!(first.uts_ns.nodename(), b"kylin-x");
    assert!(Arc::ptr_eq(first.cgroup_ns(), second.cgroup_ns()));
    assert!(Arc::ptr_eq(first.cgroup_ns(), &CgroupNamespace::initial()));
}

#[def_test]
fn test_nsproxy_clone_without_flags_shares_namespaces() {
    let parent = make_nsproxy();
    let mut fs = make_fs(&parent);
    let child = parent
        .clone_for_child(
            NamespaceFlags::empty(),
            NamespaceFsContext::Private(&mut fs),
        )
        .unwrap();

    assert!(Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
    assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
    assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
}

#[def_test]
fn test_nsproxy_clone_with_shared_fs_context_shares_mnt_namespace() {
    let parent = make_nsproxy();
    // CLONE_FS (shared fs_struct, no NEWNS): namespace references are still shared.
    let child = parent
        .clone_for_child(NamespaceFlags::empty(), NamespaceFsContext::Shared)
        .unwrap();

    assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
}

#[def_test]
fn test_nsproxy_clone_newuts_creates_new_namespace() {
    let parent = make_nsproxy();
    parent.uts_ns.set_nodename(b"original").unwrap();

    let child = parent
        .clone_for_child(NamespaceFlags::NEWUTS, NamespaceFsContext::Shared)
        .unwrap();

    // Child gets a new UTS namespace with copied values
    assert!(!Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
    assert_eq!(child.uts_ns.nodename(), b"original");

    // Modifying child does not affect parent
    child.uts_ns.set_nodename(b"modified").unwrap();
    assert_eq!(parent.uts_ns.nodename(), b"original");
    assert_eq!(child.uts_ns.nodename(), b"modified");
}

#[def_test]
fn test_nsproxy_clone_newipc_creates_new_namespace() {
    let parent = make_nsproxy();
    let child = parent
        .clone_for_child(NamespaceFlags::NEWIPC, NamespaceFsContext::Shared)
        .unwrap();

    assert!(!Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
}

#[def_test]
fn test_nsproxy_clone_newns_creates_new_mnt_namespace() {
    let parent = make_nsproxy();
    let child = clone_with_private_fs(&parent, NamespaceFlags::NEWNS);

    assert!(!Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    assert!(!Arc::ptr_eq(
        parent.mnt_ns.root_mount(),
        child.mnt_ns.root_mount()
    ));
}

#[def_test]
fn test_nsproxy_clone_newns_with_clone_fs_rejected() {
    let parent = make_nsproxy();
    let result = parent.clone_for_child(NamespaceFlags::NEWNS, NamespaceFsContext::Shared);
    assert!(result.is_err());
}

#[def_test]
fn test_nsproxy_clone_unimplemented_flags_rejected() {
    let parent = make_nsproxy();
    assert!(
        parent
            .clone_for_child(NamespaceFlags::NEWNET, NamespaceFsContext::Shared)
            .is_err()
    );
    assert!(
        parent
            .clone_for_child(NamespaceFlags::NEWUSER, NamespaceFsContext::Shared)
            .is_err()
    );
    assert!(
        parent
            .clone_for_child(NamespaceFlags::NEWPID, NamespaceFsContext::Shared)
            .is_err()
    );
    assert!(
        parent
            .clone_for_child(NamespaceFlags::NEWCGROUP, NamespaceFsContext::Shared)
            .is_err()
    );
}

#[def_test]
fn test_nsproxy_clone_preserves_shared_namespaces() {
    let parent = make_nsproxy();
    let child = parent
        .clone_for_child(NamespaceFlags::NEWUTS, NamespaceFsContext::Shared)
        .unwrap();

    // Only UTS is new; everything else is shared
    assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
    assert!(Arc::ptr_eq(&parent.net_ns, &child.net_ns));
    assert!(Arc::ptr_eq(
        &parent.pid_ns_for_children,
        &child.pid_ns_for_children
    ));
    assert!(Arc::ptr_eq(
        &parent.time_ns_for_children,
        &child.time_ns_for_children
    ));
}
