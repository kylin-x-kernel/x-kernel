// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic VFS helpers corresponding to `fs/libfs.c`.

use core::ptr;

use memaddr::PAGE_SIZE_4K;

use crate::{
    AddressSpace, Dentry, GetattrQueryFlags, GetattrRequestMask, Metadata, MountIdmap, Path,
    RenameFlags, StatFs, StatFsFlags, VfsError, VfsFile, VfsInode, VfsResult, WriteEndRequest,
    d_really_is_positive, path::MAX_NAME_LEN,
};

/// `simple_statfs` with explicit superblock mount flags.
pub fn simple_statfs_with_flags(fs_type: u32, mount_flags: StatFsFlags) -> StatFs {
    StatFs {
        fs_type,
        block_size: PAGE_SIZE_4K as u32,
        blocks: 100,
        blocks_free: 100,
        blocks_available: 100,
        file_count: 0,
        free_file_count: 0,
        name_length: MAX_NAME_LEN as u32,
        fragment_size: 0,
        mount_flags,
    }
}

/// `simple_empty`: report whether a directory has positive children.
pub(crate) fn simple_empty(dentry: &Dentry) -> VfsResult<bool> {
    Ok(!dentry.has_positive_children())
}

/// `__simple_unlink`.
pub(crate) fn __simple_unlink(_dir: &VfsInode, dentry: &Dentry) {
    dentry.decrement_link_count();
}

/// `simple_unlink`.
pub(crate) fn simple_unlink(dir: &VfsInode, dentry: &Dentry) -> VfsResult<()> {
    __simple_unlink(dir, dentry);
    Ok(())
}

/// `simple_rename_timestamp`.
pub(crate) fn simple_rename_timestamp(
    old_dir: &VfsInode,
    old_dentry: &Dentry,
    new_dir: &VfsInode,
    new_dentry: &Dentry,
) {
    let timestamp = old_dir.set_changed_at_to_now();
    old_dir.set_modified_at(timestamp);
    if !ptr::eq(old_dir, new_dir) {
        let timestamp = new_dir.set_changed_at_to_now();
        new_dir.set_modified_at(timestamp);
    }
    old_dentry.set_changed_at_to_now();
    if d_really_is_positive(new_dentry) {
        new_dentry.set_changed_at_to_now();
    }
}

/// `simple_rename_exchange`.
pub(crate) fn simple_rename_exchange(
    old_dir: &VfsInode,
    old_dentry: &Dentry,
    new_dir: &VfsInode,
    new_dentry: &Dentry,
) -> VfsResult<()> {
    let old_is_dir = old_dentry.is_dir();
    let new_is_dir = new_dentry.is_dir();

    if !ptr::eq(old_dir, new_dir) && old_is_dir != new_is_dir {
        if old_is_dir {
            old_dir.decrement_link_count();
            new_dir.increment_link_count();
        } else {
            new_dir.decrement_link_count();
            old_dir.increment_link_count();
        }
    }
    simple_rename_timestamp(old_dir, old_dentry, new_dir, new_dentry);
    Ok(())
}

/// `simple_rename`.
pub fn simple_rename(
    _idmap: &MountIdmap,
    old_dir: &VfsInode,
    old_dentry: &Dentry,
    new_dir: &VfsInode,
    new_dentry: &Dentry,
    flags: RenameFlags,
) -> VfsResult<()> {
    let they_are_dirs = old_dentry.is_dir();

    let supported_flags = RenameFlags::NOREPLACE | RenameFlags::EXCHANGE;
    if !supported_flags.contains(flags) || flags.has_conflicting_modes() {
        return Err(VfsError::InvalidInput);
    }

    if flags.contains(RenameFlags::EXCHANGE) {
        return simple_rename_exchange(old_dir, old_dentry, new_dir, new_dentry);
    }
    if flags.contains(RenameFlags::NOREPLACE) && d_really_is_positive(new_dentry) {
        return Err(VfsError::AlreadyExists);
    }

    if !simple_empty(new_dentry)? {
        return Err(VfsError::DirectoryNotEmpty);
    }

    if d_really_is_positive(new_dentry) {
        simple_unlink(new_dir, new_dentry)?;
        if they_are_dirs {
            new_dentry.decrement_link_count();
            old_dir.decrement_link_count();
        }
    } else if they_are_dirs {
        old_dir.decrement_link_count();
        new_dir.increment_link_count();
    }

    simple_rename_timestamp(old_dir, old_dentry, new_dir, new_dentry);
    Ok(())
}

/// `simple_getattr`: fill attributes from the VFS inode.
pub fn simple_getattr(
    _idmap: &MountIdmap,
    path: Option<&Path>,
    _request_mask: GetattrRequestMask,
    _query_flags: GetattrQueryFlags,
) -> VfsResult<Metadata> {
    let path = path.ok_or(VfsError::InvalidInput)?;
    let inode = path.inode();
    let mut stat = inode.metadata();
    stat.blocks = inode
        .address_space()
        .nrpages()
        .saturating_mul((PAGE_SIZE_4K >> 9) as u64);
    Ok(stat)
}

/// `simple_fsync_noflush`: write dirty file data without forcing a device flush.
pub fn simple_fsync_noflush(file: &VfsFile, data_only: bool) -> VfsResult<()> {
    file.writeback_mapping(data_only)
}

/// `simple_write_end`: finish a buffered write and extend `inode::i_size`.
pub fn simple_write_end(mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
    let copied = request.copied();
    let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
    let old_size = inode.size();
    let last_pos = request
        .pos()
        .checked_add(copied as u64)
        .ok_or(VfsError::InvalidInput)?;

    let new_size = if copied == 0 {
        old_size
    } else {
        old_size.max(last_pos)
    };
    inode.update_size_after_backing_change(new_size)?;
    Ok(copied)
}

/// `generic_read_dir`: direct byte reads on directories fail.
pub(crate) fn generic_read_dir(_buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
    Err(VfsError::IsADirectory)
}

/// `noop_fsync`: generic fsync implementation for in-memory metadata.
pub(crate) fn noop_fsync(_data_only: bool) -> VfsResult<()> {
    Ok(())
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::String, sync::Arc};
    use core::{any::Any, time::Duration};

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
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
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
            .with_stat_data(4096, 0, Duration::ZERO, Duration::ZERO, Duration::ZERO)
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
            &old_dentry,
            &new_dir,
            &new_dentry,
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
                &old_dentry,
                &new_dir,
                &new_dentry,
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
            &old_dentry,
            &new_dir,
            &new_dentry,
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
        let old_dentry =
            Dentry::new_file_from_inode(source, Some(old_parent), String::from("source"));
        let new_dentry = Dentry::new_negative(Some(new_parent), String::from("target"));

        assert_eq!(
            simple_rename(
                &MountIdmap,
                &old_dir,
                &old_dentry,
                &new_dir,
                &new_dentry,
                RenameFlags::WHITEOUT
            ),
            Err(VfsError::InvalidInput)
        );
    }

    #[def_test]
    fn simple_rename_rejects_conflicting_modes() {
        let old_dir = dir_inode(30, 2);
        let new_dir = dir_inode(31, 2);
        let source = file_inode(32, 1);
        let old_parent = Dentry::new_dir_from_inode(old_dir.clone(), None, String::from("old"));
        let new_parent = Dentry::new_dir_from_inode(new_dir.clone(), None, String::from("new"));
        let old_dentry =
            Dentry::new_file_from_inode(source, Some(old_parent), String::from("source"));
        let new_dentry = Dentry::new_negative(Some(new_parent), String::from("target"));

        assert_eq!(
            simple_rename(
                &MountIdmap,
                &old_dir,
                &old_dentry,
                &new_dir,
                &new_dentry,
                RenameFlags::NOREPLACE | RenameFlags::EXCHANGE,
            ),
            Err(VfsError::InvalidInput)
        );
    }
}
