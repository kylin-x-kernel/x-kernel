// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{
    DirMapping, FileSystemType, Filename, Mount, NodePermission, OpenFlags, SimpleDir, SimpleFs,
    VfsError,
};
use unittest::{assert_eq, def_test};

use super::{add_root_entries, add_sys_entries};

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("procfs-net-test");

#[def_test(user)]
fn ip_forward_checks_current_capability_and_namespace_isolation() {
    let thread = kprocess::current_user_thread();
    let process = thread.process();
    let original = process.net_ns().unwrap();
    let previous = knet::netns::ipv4_forwarding(&original).unwrap();
    let namespace = Arc::new(kns::NetNamespace::new());
    knet::netns::ensure_stack(&namespace).unwrap();
    let proxy = process
        .nsproxy()
        .unwrap()
        .with_namespace(&kns::NsFd::Net(namespace.clone()))
        .unwrap();
    process.apply_setns(proxy, None).unwrap();
    assert_eq!(knet::netns::ipv4_forwarding(&namespace).unwrap(), 0);
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        let mut sys = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        add_sys_entries(&mut sys, fs.clone());
        root.add("sys", SimpleDir::new_maker(fs.clone(), Arc::new(sys)));
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let open = || {
        Filename::new("/sys/net/ipv4/ip_forward")
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::READ_WRITE.bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap()
    };
    let file = open();
    assert_eq!(file.write_from(b"1\n", &mut 0), Ok(2));
    let mut buf = [0; 8];
    assert_eq!(file.read_from(&mut buf, &mut 0), Ok(2));
    assert_eq!(&buf[..2], b"1\n");
    assert_eq!(knet::netns::ipv4_forwarding(&original).unwrap(), previous);
    let next_namespace = Arc::new(kns::NetNamespace::new());
    knet::netns::ensure_stack(&next_namespace).unwrap();
    let proxy = process
        .nsproxy()
        .unwrap()
        .with_namespace(&kns::NsFd::Net(next_namespace.clone()))
        .unwrap();
    process.apply_setns(proxy, None).unwrap();
    let next_file = open();
    assert_eq!(next_file.write_from(b"2\n", &mut 0), Ok(2));
    assert_eq!(file.read_from(&mut buf, &mut 0), Ok(2));
    assert_eq!(&buf[..2], b"1\n");
    thread.commit_creds(kcred::Cred::new(1000, 1000));
    assert_eq!(
        file.write_from(b"0\n", &mut 0),
        Err(VfsError::OperationNotPermitted)
    );
    thread.commit_creds(kcred::Cred::root());
    assert_eq!(file.write_from(b"0\n", &mut 0), Ok(2));
    assert_eq!(knet::netns::ipv4_forwarding(&namespace).unwrap(), 0);
    assert_eq!(next_file.read_from(&mut buf, &mut 0), Ok(2));
    assert_eq!(&buf[..2], b"2\n");
    assert_eq!(knet::netns::ipv4_forwarding(&next_namespace).unwrap(), 2);
}
