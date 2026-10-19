// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcred::initial_cred;
use kvfs::{DirMapping, NodeType, SimpleDir, SimpleFs};
use unittest::{assert, assert_eq, def_test};

use super::*;

static TEST_FILE_SYSTEM_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("kns-nsfd");

fn test_mnt_namespace() -> Arc<MntNamespace> {
    let root_fs = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        root.add_dir(
            "mnt",
            SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    crate::MntNamespace::new_root(&root_fs, kcred::initial_user_namespace())
}

fn test_fd(kind: NamespaceType) -> NsFd {
    match kind {
        NamespaceType::Uts => NsFd::Uts(Arc::new(UtsNamespace::new())),
        NamespaceType::Ipc => NsFd::Ipc(Arc::new(IpcNamespace::new())),
        NamespaceType::Mnt => NsFd::Mnt(test_mnt_namespace()),
        NamespaceType::Pid => NsFd::Pid(kidentity::root_pid_namespace().clone()),
        NamespaceType::Net => NsFd::Net(Arc::new(NetNamespace::new())),
        NamespaceType::User => NsFd::User(kcred::initial_user_namespace()),
        NamespaceType::Cgroup => NsFd::Cgroup(kcgroup::CgroupNamespace::initial()),
        NamespaceType::Time => unreachable!("no NsFd variant for the time namespace"),
    }
}

#[def_test]
fn nsfd_type_and_flag_match() {
    assert_eq!(test_fd(NamespaceType::Uts).ns_type(), NamespaceType::Uts);
    assert_eq!(
        test_fd(NamespaceType::Uts).namespace_flag(),
        NamespaceFlags::NEWUTS
    );
    assert_eq!(
        test_fd(NamespaceType::Mnt).namespace_flag(),
        NamespaceFlags::NEWNS
    );
    assert_eq!(
        test_fd(NamespaceType::Pid).namespace_flag(),
        NamespaceFlags::NEWPID
    );
}

#[def_test]
fn nsfd_anon_names_match_linux_convention() {
    assert_eq!(anon_inode_name(NamespaceType::Uts), "[uts_ns]");
    assert_eq!(anon_inode_name(NamespaceType::Net), "[net_ns]");
    assert_eq!(anon_inode_name(NamespaceType::Pid), "[pid_ns_for_children]");
}

#[def_test(serial)]
fn nsfd_file_roundtrips_private_data() {
    anon_inodefs::init_anon_inodefs();
    let nsfd = test_fd(NamespaceType::Uts);
    let file = nsfd
        .new_file(0, initial_cred())
        .expect("namespace fd opens");
    let resolved = NsFd::from_file(&file).expect("namespace fd resolves");
    assert_eq!(resolved.ns_type(), NamespaceType::Uts);
    let mode = file.path().getattr().unwrap().mode;
    assert_eq!(mode.node_type(), NodeType::Unknown);
    assert!(file.mode().contains(kvfs::FMode::READ));
}
