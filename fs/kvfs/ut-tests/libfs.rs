// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};
use core::any::Any;

use ktime_types::SystemTime;
use unittest::def_test;

use super::*;
use crate::{
    DeviceId, FileOperations, InodeOperations, Metadata, NodeFlags, NodePermission, NodeType,
    Umode, VfsInodeInit,
};

struct TestOps {
    inode: u64,
    node_type: NodeType,
    nlink: u64,
}

impl TestOps {
    fn new(inode: u64, node_type: NodeType, nlink: u64) -> Self {
        Self {
            inode,
            node_type,
            nlink,
        }
    }
}

impl InodeOperations for TestOps {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: self.inode,
            nlink: self.nlink,
            mode: Umode::new(self.node_type, NodePermission::default()),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        })
    }
}

impl FileOperations for TestOps {}

fn init(inode: u64, node_type: NodeType, nlink: u64) -> VfsInodeInit {
    let mode = match node_type {
        NodeType::Directory => NodePermission::from_bits_truncate(0o777),
        _ => NodePermission::default(),
    };
    VfsInodeInit::new(inode, 0, Umode::new(node_type, mode))
        .with_owner_links_and_rdev(0, 0, nlink, DeviceId::default())
        .with_stat_data(
            4096,
            0,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
        )
}

fn dir_inode(inode: u64, nlink: u64) -> Arc<VfsInode> {
    VfsInode::new_openable_dir_with_flags(
        Arc::new(TestOps::new(inode, NodeType::Directory, nlink)),
        NodeFlags::empty(),
        init(inode, NodeType::Directory, nlink),
    )
}

fn file_inode(inode: u64, nlink: u64) -> Arc<VfsInode> {
    let ops = Arc::new(TestOps::new(inode, NodeType::RegularFile, nlink));
    let i_private: Arc<dyn Any + Send + Sync> = ops.clone();
    let inode_operations: Arc<dyn InodeOperations> = ops.clone();
    let file_operations: Arc<dyn FileOperations> = ops;
    VfsInode::new_file_with_operations(
        i_private,
        inode_operations,
        file_operations,
        NodeFlags::empty(),
        init(inode, NodeType::RegularFile, nlink),
    )
}

#[def_test]
fn simple_rename_replaces_positive_target() {
    let old_dir = dir_inode(1, 2);
    let new_dir = dir_inode(2, 2);
    let source = file_inode(3, 1);
    let target = file_inode(4, 1);
    let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
    let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
    let old_dentry =
        Dentry::new_file_from_inode(source.clone(), Some(old_parent), String::from("source"));
    let new_dentry =
        Dentry::new_file_from_inode(target.clone(), Some(new_parent), String::from("target"));

    simple_rename(
        &MountIdmap,
        &old_dir,
        &old_dentry.lock_location(),
        &new_dir,
        &new_dentry.lock_location(),
        RenameFlags::empty(),
    )
    .unwrap();

    assert_eq!(source.link_count(), 1);
    assert_eq!(target.link_count(), 0);
    assert_eq!(old_dir.link_count(), 2);
    assert_eq!(new_dir.link_count(), 2);
}

#[def_test]
fn simple_rename_noreplace_rejects_positive_target() {
    let old_dir = dir_inode(1, 2);
    let new_dir = dir_inode(2, 2);
    let source = file_inode(3, 1);
    let target = file_inode(4, 1);
    let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
    let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
    let old_dentry =
        Dentry::new_file_from_inode(source.clone(), Some(old_parent), String::from("source"));
    let new_dentry =
        Dentry::new_file_from_inode(target.clone(), Some(new_parent), String::from("target"));

    assert_eq!(
        simple_rename(
            &MountIdmap,
            &old_dir,
            &old_dentry.lock_location(),
            &new_dir,
            &new_dentry.lock_location(),
            RenameFlags::NOREPLACE,
        ),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(source.link_count(), 1);
    assert_eq!(target.link_count(), 1);
}

#[def_test]
fn simple_rename_directory_to_negative_target_updates_parent_links() {
    let old_dir = dir_inode(10, 2);
    let new_dir = dir_inode(11, 2);
    let source = dir_inode(12, 2);
    let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
    let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
    let old_dentry =
        Dentry::new_dir_from_inode(source.clone(), Some(old_parent), String::from("source"));
    let new_dentry = Dentry::new_negative(Some(new_parent), String::from("target"));

    simple_rename(
        &MountIdmap,
        &old_dir,
        &old_dentry.lock_location(),
        &new_dir,
        &new_dentry.lock_location(),
        RenameFlags::empty(),
    )
    .unwrap();

    assert_eq!(source.link_count(), 2);
    assert_eq!(old_dir.link_count(), 1);
    assert_eq!(new_dir.link_count(), 3);
}

#[def_test]
fn simple_empty_accepts_negative_dentry() {
    let negative = Dentry::new_negative(None, String::from("target"));

    assert_eq!(simple_empty(&negative), Ok(true));
}

#[def_test]
fn simple_rename_rejects_unsupported_flags() {
    let old_dir = dir_inode(20, 2);
    let new_dir = dir_inode(21, 2);
    let source = file_inode(22, 1);
    let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
    let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
    let old_dentry = Dentry::new_file_from_inode(source, Some(old_parent), String::from("source"));
    let new_dentry = Dentry::new_negative(Some(new_parent), String::from("target"));

    assert_eq!(
        simple_rename(
            &MountIdmap,
            &old_dir,
            &old_dentry.lock_location(),
            &new_dir,
            &new_dentry.lock_location(),
            RenameFlags::WHITEOUT,
        ),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn simple_rename_rejects_noreplace_exchange_combination() {
    let old_dir = dir_inode(30, 2);
    let new_dir = dir_inode(31, 2);
    let source = file_inode(32, 1);
    let target = file_inode(33, 1);
    let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
    let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
    let old_dentry =
        Dentry::new_file_from_inode(source.clone(), Some(old_parent), String::from("source"));
    let new_dentry =
        Dentry::new_file_from_inode(target.clone(), Some(new_parent), String::from("target"));

    assert_eq!(
        simple_rename(
            &MountIdmap,
            &old_dir,
            &old_dentry.lock_location(),
            &new_dir,
            &new_dentry.lock_location(),
            RenameFlags::NOREPLACE | RenameFlags::EXCHANGE,
        ),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(source.link_count(), 1);
    assert_eq!(target.link_count(), 1);
}
