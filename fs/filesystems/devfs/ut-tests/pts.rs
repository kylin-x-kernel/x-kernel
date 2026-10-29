// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use kvfs::{DeviceId, DirMapping, SimpleDir};
use unittest::def_test;

use super::*;
use crate::DeviceFile;

struct OccupiedDevice;
impl DeviceFileOps for OccupiedDevice {}

struct RegisteredDevice(DeviceId);

impl Drop for RegisteredDevice {
    fn drop(&mut self) {
        drop(kvfs::cdev_del(self.0, 1));
    }
}

fn test_filesystem() -> Arc<SimpleFs> {
    let super_block = SimpleFs::new_with(&crate::FILE_SYSTEM_TYPE, crate::DEVFS_MAGIC, |fs| {
        SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
    });
    super_block.private::<Arc<SimpleFs>>().unwrap().clone()
}

#[def_test(serial)]
fn retained_slave_node_does_not_own_pair_or_number() {
    let super_block = SimpleFs::new_with(&crate::FILE_SYSTEM_TYPE, crate::DEVFS_MAGIC, |fs| {
        SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
    });
    let fs = super_block.private::<Arc<SimpleFs>>().unwrap().clone();
    struct RetainedSlave {
        inode: Arc<VfsInode>,
        pair: Weak<PtyPair>,
    }

    struct FailedNodeConstruction {
        fs: Arc<SimpleFs>,
        retained: ksync::Mutex<Vec<RetainedSlave>>,
    }

    impl FileOperations for FailedNodeConstruction {
        fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
            tty::open_ptmx(&self.fs, inode, file, |pair| {
                let inode = SlaveNode::inode(self.fs.clone(), pair);
                assert_eq!(inode.rdev(), pair.slave_device_id());
                self.retained.lock().push(RetainedSlave {
                    inode,
                    pair: Arc::downgrade(pair),
                });
                Err(KError::InvalidInput)
            })
        }
    }

    let operations = Arc::new(FailedNodeConstruction {
        fs: fs.clone(),
        retained: ksync::Mutex::new(Vec::new()),
    });
    let node = Arc::new(SimpleFsNode::new(
        fs,
        NodeType::RegularFile,
        NodePermission::default(),
    ));
    let inode = VfsInode::new_file_with_operations(
        node.clone(),
        node.clone(),
        operations.clone(),
        NodeFlags::PRIVATE,
        node.inode_init(),
    );
    let mount = kvfs::Mount::new_root(&super_block);
    let dentry =
        Dentry::new_file_from_inode(inode, Some(super_block.root_dir()), "ptmx-failure".into());
    let path = kvfs::Path::new(mount, dentry);
    for _ in 0..2 {
        assert_eq!(
            kvfs::dentry_open(path.clone(), 0, kcred::initial_cred()).err(),
            Some(KError::InvalidInput)
        );
    }
    let retained = operations.retained.lock();
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().all(|slave| slave.pair.upgrade().is_none()));
    assert_eq!(retained[0].inode.rdev(), retained[1].inode.rdev());
}

#[def_test(serial)]
fn device_node_conflict_does_not_publish_or_replace_registration() {
    let fs = test_filesystem();
    let number = DeviceId::new(242, 40);
    let original: Arc<dyn DeviceFileOps> = Arc::new(OccupiedDevice);
    kvfs::cdev_add(number, 1, original.clone()).unwrap();
    let registration = RegisteredDevice(number);
    let mut directory = DirMapping::new();
    crate::add_device_entry(
        &mut directory,
        "conflict",
        DeviceFile::new_character(fs, number, Arc::new(OccupiedDevice)),
    );
    assert_eq!(directory.child_names().unwrap().count(), 0);
    let removed = kvfs::cdev_del(number, 1).unwrap();
    assert!(Arc::ptr_eq(&removed, &original));
    drop(registration);
}
