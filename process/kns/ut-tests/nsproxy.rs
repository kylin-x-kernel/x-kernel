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
    // NEWNET is rejected: knet still resolves sockets through its global
    // protocol stack, so a fresh identity alone would be pseudo-isolation.
    for flags in [
        NamespaceFlags::NEWNET,
        NamespaceFlags::NEWUSER,
        NamespaceFlags::NEWCGROUP,
    ] {
        assert!(
            parent
                .clone_for_child(flags, NamespaceFsContext::Shared)
                .is_err(),
            "clone must reject {flags:?} while the data plane is global"
        );
    }
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

#[def_test]
fn test_nsproxy_clone_newpid_creates_child_pid_namespace() {
    let parent = make_nsproxy();
    let child = parent
        .clone_for_child(NamespaceFlags::NEWPID, NamespaceFsContext::Shared)
        .unwrap();

    assert!(!Arc::ptr_eq(
        &parent.pid_ns_for_children,
        &child.pid_ns_for_children
    ));
    // The child namespace descends from the parent's current pid namespace.
    let parent_of_child = child
        .pid_ns_for_children
        .parent()
        .expect("child ns has parent");
    assert!(Arc::ptr_eq(parent_of_child, &parent.pid_ns_for_children));
    // Everything else is shared.
    assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    assert!(Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
}

#[def_test]
fn test_unshare_prepare_rejects_unsupported_and_unprivileged() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    let context = UnshareContext {
        privileged: true,
        fs_shared_with_others: false,
        unshare_fs: false,
        unshare_files: false,
    };

    // NEWNET is rejected like the other unimplemented namespaces: knet still
    // resolves sockets through its global protocol stack, so a fresh identity
    // alone would be pseudo-isolation.
    for flags in [
        NamespaceFlags::NEWNET,
        NamespaceFlags::NEWUSER,
        NamespaceFlags::NEWCGROUP,
        NamespaceFlags::NEWTIME,
    ] {
        assert!(
            matches!(
                proxy.prepare_unshare(flags, context, &fs),
                Err(UnshareError::Unsupported)
            ),
            "{flags:?} must stay explicitly unsupported, not pseudo-isolated"
        );
    }

    // Without privilege, namespace creation is rejected with EPERM.
    assert!(matches!(
        proxy.prepare_unshare(
            NamespaceFlags::NEWUTS,
            UnshareContext {
                privileged: false,
                ..context
            },
            &fs
        ),
        Err(UnshareError::NotPermitted)
    ));
    // CLONE_FS / CLONE_FILES alone do not need privilege.
    assert!(
        proxy
            .prepare_unshare(
                NamespaceFlags::empty(),
                UnshareContext {
                    privileged: false,
                    unshare_fs: true,
                    unshare_files: true,
                    ..context
                },
                &fs
            )
            .is_ok()
    );
}

#[def_test]
fn test_unshare_prepare_newns_rejects_shared_fs_without_clone_fs() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    assert!(matches!(
        proxy.prepare_unshare(
            NamespaceFlags::NEWNS,
            UnshareContext {
                privileged: true,
                fs_shared_with_others: true,
                unshare_fs: false,
                unshare_files: false,
            },
            &fs
        ),
        Err(UnshareError::InvalidArgument)
    ));

    // CLONE_FS lifts the shared-fs restriction.
    let prepared = proxy
        .prepare_unshare(
            NamespaceFlags::NEWNS,
            UnshareContext {
                privileged: true,
                fs_shared_with_others: true,
                unshare_fs: true,
                unshare_files: false,
            },
            &fs,
        )
        .unwrap();
    assert!(!Arc::ptr_eq(&proxy.mnt_ns, &prepared.nsproxy.mnt_ns));
    assert!(prepared.new_fs.is_some());
}

#[def_test]
fn test_unshare_prepare_copies_requested_namespaces() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    let prepared = proxy
        .prepare_unshare(
            NamespaceFlags::NEWUTS | NamespaceFlags::NEWIPC | NamespaceFlags::NEWPID,
            UnshareContext {
                privileged: true,
                fs_shared_with_others: false,
                unshare_fs: false,
                unshare_files: true,
            },
            &fs,
        )
        .unwrap();

    assert!(!Arc::ptr_eq(&proxy.uts_ns, &prepared.nsproxy.uts_ns));
    assert!(!Arc::ptr_eq(&proxy.ipc_ns, &prepared.nsproxy.ipc_ns));
    assert!(!Arc::ptr_eq(
        &proxy.pid_ns_for_children,
        &prepared.nsproxy.pid_ns_for_children
    ));
    // Unselected references stay shared.
    assert!(Arc::ptr_eq(&proxy.mnt_ns, &prepared.nsproxy.mnt_ns));
    assert!(Arc::ptr_eq(&proxy.cgroup_ns, &prepared.nsproxy.cgroup_ns));
    // CLONE_FILES is recorded for the commit phase; no fs copy was drafted.
    assert!(prepared.unshare_files());
    assert!(prepared.new_fs().is_none());
}

#[def_test]
fn test_unshare_prepare_clone_fs_without_newns_detaches_fs_struct() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    let prepared = proxy
        .prepare_unshare(
            NamespaceFlags::empty(),
            UnshareContext {
                privileged: false,
                fs_shared_with_others: false,
                unshare_fs: true,
                unshare_files: false,
            },
            &fs,
        )
        .unwrap();

    // The mount namespace is shared; a private fs copy is drafted.
    assert!(Arc::ptr_eq(&proxy.mnt_ns, &prepared.nsproxy.mnt_ns));
    assert!(prepared.new_fs().is_some());
}

#[def_test]
fn test_with_namespace_swaps_only_requested_slot() {
    let proxy = make_nsproxy();
    let other_uts = Arc::new(UtsNamespace::new());
    other_uts.set_nodename(b"other").unwrap();

    let mut fs = make_fs(&proxy);
    let child_proxy = proxy
        .clone_for_child(NamespaceFlags::NEWPID, NamespaceFsContext::Private(&mut fs))
        .unwrap();

    // Re-enter the ORIGINAL pid namespace via an NsFd pinned to the parent.
    let swapped = child_proxy
        .with_namespace(&NsFd::Pid(proxy.pid_ns_for_children().clone()))
        .unwrap();
    assert!(Arc::ptr_eq(
        swapped.pid_ns_for_children(),
        &proxy.pid_ns_for_children
    ));
    assert!(Arc::ptr_eq(&swapped.mnt_ns, &child_proxy.mnt_ns));

    // UTS swap keeps the pid slot.
    let uts_swapped = child_proxy
        .with_namespace(&NsFd::Uts(other_uts.clone()))
        .unwrap();
    assert!(Arc::ptr_eq(&uts_swapped.uts_ns, &other_uts));
    assert!(Arc::ptr_eq(
        &uts_swapped.pid_ns_for_children,
        &child_proxy.pid_ns_for_children
    ));
}

#[def_test]
fn test_with_namespace_rejects_pseudo_isolation_kinds() {
    let proxy = make_nsproxy();
    // Net is rejected like the other pseudo-isolation kinds: knet still
    // resolves sockets through its global protocol stack, so switching only
    // the namespace reference would not isolate anything.
    for nsfd in [
        NsFd::Net(Arc::new(crate::NetNamespace::new())),
        NsFd::User(kcred::initial_user_namespace()),
        NsFd::Cgroup(kcgroup::CgroupNamespace::initial()),
    ] {
        assert!(matches!(
            proxy.with_namespace(&nsfd),
            Err(UnshareError::Unsupported)
        ));
    }
}
