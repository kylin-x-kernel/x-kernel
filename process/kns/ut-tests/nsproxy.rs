// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kcgroup::{Cgroup, CgroupNamespace};
use kcred::{Cred, initial_user_namespace};
use klazy::Once;
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
    })
    .unwrap();
    MntNamespace::new_root(&root_fs, initial_user_namespace())
}

fn test_cgroup() -> Arc<Cgroup> {
    static ROOT: Once<Arc<Cgroup>> = Once::new();
    ROOT.call_once(|| CgroupNamespace::new().root()).clone()
}

fn make_nsproxy() -> Arc<NsProxy> {
    NsProxy::new_initial_with_mnt_ns(make_mnt_namespace())
}

fn make_fs(nsproxy: &NsProxy) -> FsStruct {
    FsStruct::new(nsproxy.mnt_ns().visible_root_path())
}

fn test_cred() -> Arc<Cred> {
    Arc::new(Cred::root())
}

fn clone_with_private_fs(parent: &NsProxy, flags: NamespaceFlags) -> Arc<NsProxy> {
    let mut fs = make_fs(parent);
    parent
        .clone_for_child(
            flags,
            NamespaceFsContext::Private(&mut fs),
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0
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
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

    assert!(Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
    assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
    assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
}

#[def_test]
fn test_nsproxy_clone_with_shared_fs_context_shares_mnt_namespace() {
    let parent = make_nsproxy();
    // CLONE_FS (shared fs_struct, no NEWNS): namespace references are still shared.
    let child = parent
        .clone_for_child(
            NamespaceFlags::empty(),
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

    assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
}

#[def_test]
fn test_nsproxy_clone_newuts_creates_new_namespace() {
    let parent = make_nsproxy();
    parent.uts_ns.set_nodename(b"original").unwrap();

    let child = parent
        .clone_for_child(
            NamespaceFlags::NEWUTS,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

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
        .clone_for_child(
            NamespaceFlags::NEWIPC,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

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
    let result = parent.clone_for_child(
        NamespaceFlags::NEWNS,
        NamespaceFsContext::Shared,
        &test_cred(),
        Some(&test_cgroup()),
    );
    assert!(result.is_err());
}

#[def_test]
fn test_nsproxy_clone_materializes_supported_namespaces() {
    let parent = make_nsproxy();
    // NEWNET is supported: the child receives a fresh namespace identity.
    let net_child = parent
        .clone_for_child(
            NamespaceFlags::NEWNET,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;
    assert_ne!(net_child.net_ns().id(), parent.net_ns().id());

    // CLONE_NEWUSER returns the new user namespace for the credential commit;
    // the namespace bundle itself has no user-namespace slot.
    let (_, user_ns) = parent
        .clone_for_child(
            NamespaceFlags::NEWUSER,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap();
    let user_ns = user_ns.expect("CLONE_NEWUSER must report the new user namespace");
    assert!(Arc::ptr_eq(
        user_ns.parent().expect("child user namespace has a parent"),
        &initial_user_namespace()
    ));

    // CLONE_NEWCGROUP roots a new view at the caller's current cgroup; the
    // hierarchy itself is shared, so no new controller state is created.
    let cgroup_child = parent
        .clone_for_child(
            NamespaceFlags::NEWCGROUP,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;
    assert_ne!(cgroup_child.cgroup_ns().id(), parent.cgroup_ns().id());
    assert!(Arc::ptr_eq(
        &cgroup_child.cgroup_ns().root(),
        &test_cgroup()
    ));
}

#[def_test]
fn test_nsproxy_clone_preserves_shared_namespaces() {
    let parent = make_nsproxy();
    let child = parent
        .clone_for_child(
            NamespaceFlags::NEWUTS,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

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
        .clone_for_child(
            NamespaceFlags::NEWPID,
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

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
fn test_unshare_prepare_creates_user_time_and_net_namespaces() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    let context = UnshareContext {
        cred: test_cred(),
        fs_shared_with_others: false,
        unshare_fs: false,
        unshare_files: false,
        current_cgroup: Some(test_cgroup()),
    };

    for flags in [NamespaceFlags::NEWUSER, NamespaceFlags::NEWTIME] {
        let prepared = proxy
            .prepare_unshare(flags, context.clone(), &fs)
            .unwrap_or_else(|error| panic!("unshare({flags:?}) failed: {error:?}"));
        if flags == NamespaceFlags::NEWUSER {
            assert!(prepared.user_ns.is_some());
            assert!(Arc::ptr_eq(
                &proxy.time_ns_for_children,
                &prepared.nsproxy.time_ns_for_children
            ));
        } else {
            assert!(prepared.user_ns.is_none());
            assert!(!Arc::ptr_eq(
                &proxy.time_ns_for_children,
                &prepared.nsproxy.time_ns_for_children
            ));
            assert!(Arc::ptr_eq(&proxy.time_ns, &prepared.nsproxy.time_ns));
        }
    }

    let net = proxy
        .prepare_unshare(NamespaceFlags::NEWNET, context.clone(), &fs)
        .expect("NEWNET unshare is supported");
    assert_ne!(net.nsproxy().net_ns().id(), proxy.net_ns().id());

    let cgroup = proxy
        .prepare_unshare(NamespaceFlags::NEWCGROUP, context.clone(), &fs)
        .expect("NEWCGROUP unshare is supported");
    assert_ne!(cgroup.nsproxy().cgroup_ns().id(), proxy.cgroup_ns().id());
    assert!(Arc::ptr_eq(
        &cgroup.nsproxy().cgroup_ns().root(),
        &test_cgroup()
    ));

    // Namespace creation other than NEWUSER needs CAP_SYS_ADMIN.
    let unprivileged = UnshareContext {
        cred: Arc::new(Cred::new(1000, 1000)),
        ..context.clone()
    };
    assert!(matches!(
        proxy.prepare_unshare(NamespaceFlags::NEWUTS, unprivileged.clone(), &fs),
        Err(UnshareError::NotPermitted)
    ));
    assert!(
        proxy
            .prepare_unshare(NamespaceFlags::NEWUSER, unprivileged, &fs)
            .is_ok()
    );

    // Linux creates the user namespace first, so an unprivileged caller may
    // combine CLONE_NEWUSER with namespaces that otherwise require
    // CAP_SYS_ADMIN in the newly created namespace.
    let prepared = proxy
        .prepare_unshare(
            NamespaceFlags::NEWUSER | NamespaceFlags::NEWNS,
            UnshareContext {
                cred: Arc::new(Cred::new(1000, 1000)),
                fs_shared_with_others: false,
                unshare_fs: false,
                unshare_files: false,
                current_cgroup: Some(test_cgroup()),
            },
            &fs,
        )
        .expect("NEWUSER must authorize other namespace creation");
    assert!(prepared.user_ns.is_some());
    assert!(!Arc::ptr_eq(&proxy.mnt_ns, prepared.nsproxy().mnt_ns()));

    // CLONE_FS / CLONE_FILES alone do not need privilege.
    assert!(
        proxy
            .prepare_unshare(
                NamespaceFlags::empty(),
                UnshareContext {
                    cred: Arc::new(Cred::new(1000, 1000)),
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
                cred: test_cred(),
                fs_shared_with_others: true,
                unshare_fs: false,
                unshare_files: false,
                current_cgroup: Some(test_cgroup()),
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
                cred: test_cred(),
                fs_shared_with_others: true,
                unshare_fs: true,
                unshare_files: false,
                current_cgroup: Some(test_cgroup()),
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
                cred: test_cred(),
                fs_shared_with_others: false,
                unshare_fs: false,
                unshare_files: true,
                current_cgroup: Some(test_cgroup()),
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
    let mut fs = make_fs(&proxy);
    assert!(matches!(
        fs.begin_exec(),
        fs_context::ExecAdmission::Exclusive
    ));
    let prepared = proxy
        .prepare_unshare(
            NamespaceFlags::empty(),
            UnshareContext {
                cred: Arc::new(Cred::new(1000, 1000)),
                fs_shared_with_others: false,
                unshare_fs: true,
                unshare_files: false,
                current_cgroup: Some(test_cgroup()),
            },
            &fs,
        )
        .unwrap();

    // The mount namespace is shared; a private fs copy is drafted.
    assert!(Arc::ptr_eq(&proxy.mnt_ns, &prepared.nsproxy.mnt_ns));
    let new_fs = prepared.new_fs().expect("CLONE_FS drafts a private fs");
    assert!(
        !new_fs.in_exec(),
        "a private fs copy must not inherit the parent's exec admission"
    );
    fs.end_exec();
}

#[def_test]
fn test_with_namespace_swaps_only_requested_slot() {
    let proxy = make_nsproxy();
    let other_uts = Arc::new(UtsNamespace::new());
    other_uts.set_nodename(b"other").unwrap();

    let mut fs = make_fs(&proxy);
    let child_proxy = proxy
        .clone_for_child(
            NamespaceFlags::NEWPID,
            NamespaceFsContext::Private(&mut fs),
            &test_cred(),
            Some(&test_cgroup()),
        )
        .unwrap()
        .0;

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
fn test_with_namespace_swaps_every_backed_namespace() {
    let proxy = make_nsproxy();
    // Net swaps in the target's NetNamespace.
    let net_target = Arc::new(crate::NetNamespace::new());
    let net_swapped = proxy
        .with_namespace(&NsFd::Net(net_target.clone()))
        .unwrap();
    assert!(Arc::ptr_eq(net_swapped.net_ns(), &net_target));

    // Cgroup namespaces are a view root over the existing hierarchy, so the
    // swap must replace only the view and leave every other slot shared.
    let cgroup_target = kcgroup::CgroupNamespace::initial();
    let cgroup_swapped = proxy
        .with_namespace(&NsFd::Cgroup(cgroup_target.clone()))
        .unwrap();
    assert!(Arc::ptr_eq(cgroup_swapped.cgroup_ns(), &cgroup_target));
    assert!(Arc::ptr_eq(cgroup_swapped.net_ns(), proxy.net_ns()));
    assert!(Arc::ptr_eq(cgroup_swapped.mnt_ns(), proxy.mnt_ns()));

    // User namespaces live in credentials; the bundle must stay untouched and
    // the syscall layer performs the credential transition separately.
    let user_swapped = proxy
        .with_namespace(&NsFd::User(initial_user_namespace()))
        .unwrap();
    assert!(Arc::ptr_eq(proxy.mnt_ns(), user_swapped.mnt_ns()));
    assert!(Arc::ptr_eq(proxy.uts_ns(), user_swapped.uts_ns()));
    assert!(Arc::ptr_eq(proxy.ipc_ns(), user_swapped.ipc_ns()));
    assert!(Arc::ptr_eq(
        proxy.pid_ns_for_children(),
        user_swapped.pid_ns_for_children()
    ));
    assert!(Arc::ptr_eq(proxy.net_ns(), user_swapped.net_ns()));
    assert!(Arc::ptr_eq(proxy.cgroup_ns(), user_swapped.cgroup_ns()));
    assert!(Arc::ptr_eq(proxy.time_ns(), user_swapped.time_ns()));
    assert!(Arc::ptr_eq(
        proxy.time_ns_for_children(),
        user_swapped.time_ns_for_children()
    ));
}

#[def_test]
fn test_with_namespace_replaces_both_time_slots() {
    let proxy = make_nsproxy();
    let target = Arc::new(crate::TimeNamespace::new_in(initial_user_namespace()));
    let swapped = proxy.with_namespace(&NsFd::Time(target.clone())).unwrap();
    assert!(Arc::ptr_eq(swapped.time_ns(), &target));
    assert!(Arc::ptr_eq(swapped.time_ns_for_children(), &target));
}

#[def_test]
fn test_fork_enters_time_ns_for_children() {
    let proxy = make_nsproxy();
    let fs = make_fs(&proxy);
    let context = UnshareContext {
        cred: test_cred(),
        fs_shared_with_others: false,
        unshare_fs: false,
        unshare_files: false,
        current_cgroup: Some(test_cgroup()),
    };
    let prepared = proxy
        .prepare_unshare(NamespaceFlags::NEWTIME, context, &fs)
        .expect("NEWTIME unshare is supported");
    let parent = prepared.nsproxy();
    assert!(!Arc::ptr_eq(
        &parent.time_ns,
        &parent.time_ns_for_children
    ));

    // A plain fork after unshare(CLONE_NEWTIME) must enter the namespace
    // selected for children, not the parent's old active time namespace.
    let child = parent
        .clone_for_child(
            NamespaceFlags::empty(),
            NamespaceFsContext::Shared,
            &test_cred(),
            Some(&test_cgroup()),
        )
        .expect("plain fork should clone the namespace bundle")
        .0;
    assert!(Arc::ptr_eq(
        &child.time_ns,
        &parent.time_ns_for_children
    ));
    assert!(!Arc::ptr_eq(&child.time_ns, &parent.time_ns));
}

#[def_test]
fn test_newuser_rejects_a_chrooted_fs_root() {
    let proxy = make_nsproxy();
    let base = make_fs(&proxy);
    let sub = base.root().dentry().lookup("mnt").unwrap();
    let chroot_fs = FsStruct::new(kvfs::Path::new(
        base.root().mount().clone(),
        sub,
    ));
    let context = UnshareContext {
        cred: test_cred(),
        fs_shared_with_others: false,
        unshare_fs: false,
        unshare_files: false,
        current_cgroup: Some(test_cgroup()),
    };

    assert!(matches!(
        proxy.prepare_unshare(NamespaceFlags::NEWUSER, context, &chroot_fs),
        Err(UnshareError::NotPermitted)
    ));
}

#[def_test]
fn test_newuser_depth_is_reported_before_the_chroot_error() {
    let mut namespace = kcred::initial_user_namespace();
    let writer = Cred::root();
    for _ in 0..33 {
        let child = UserNamespace::new_child(&namespace, 0, 0, false).unwrap();
        child
            .write_uid_map(&writer, &writer, alloc::vec![kcred::IdMapExtent::new(0, 0, 1).unwrap()])
            .unwrap();
        child
            .write_gid_map(&writer, &writer, alloc::vec![kcred::IdMapExtent::new(0, 0, 1).unwrap()])
            .unwrap();
        namespace = child;
    }
    let proxy = make_nsproxy();
    let base = make_fs(&proxy);
    let sub = base.root().dentry().lookup("mnt").unwrap();
    let chroot_fs = FsStruct::new(kvfs::Path::new(base.root().mount().clone(), sub));
    let context = UnshareContext {
        cred: Arc::new(Cred::new(0, 0).with_user_namespace(namespace)),
        fs_shared_with_others: false,
        unshare_fs: false,
        unshare_files: false,
        current_cgroup: Some(test_cgroup()),
    };
    assert!(matches!(
        proxy.prepare_unshare(NamespaceFlags::NEWUSER, context, &chroot_fs),
        Err(UnshareError::NoSpace)
    ));
}
