// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(unittest)]

extern crate alloc;

use alloc::{string::String, sync::Arc};
use core::{any::Any, time::Duration};

use unittest::{assert, assert_eq, def_test};

use crate::{
    DirEntry, DirEntrySink, DirNode, DirNodeOps, Filesystem, FilesystemOps, Metadata,
    MetadataUpdate, MountFlags, Mountpoint, NodeOps, NodePermission, NodeType, Reference,
    ST_NOATIME, ST_NODEV, ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_NOSYMFOLLOW, ST_RDONLY,
    ST_RELATIME, StatFs, VfsError, VfsResult,
};

struct MockFilesystem {
    mount_flags: u32,
}

impl FilesystemOps for MockFilesystem {
    fn name(&self) -> &str {
        "mockfs"
    }

    fn root_dir(&self) -> DirEntry {
        DirEntry::new_dir(
            |_| DirNode::new(Arc::new(MockDirNodeOps::new(self.mount_flags, 1))),
            Reference::root(),
        )
    }

    fn stat(&self) -> VfsResult<StatFs> {
        statfs(self.mount_flags)
    }
}

struct MockDirNodeOps {
    mount_flags: u32,
    inode: u64,
}

impl MockDirNodeOps {
    fn new(mount_flags: u32, inode: u64) -> Self {
        Self { mount_flags, inode }
    }
}

impl NodeOps for MockDirNodeOps {
    fn inode(&self) -> u64 {
        self.inode
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: NodePermission::default(),
            node_type: NodeType::Directory,
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 512,
            blocks: 1,
            rdev: Default::default(),
            atime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl FilesystemOps for MockDirNodeOps {
    fn name(&self) -> &str {
        "mockfs"
    }

    fn root_dir(&self) -> DirEntry {
        panic!("root_dir is not used through directory nodes")
    }

    fn stat(&self) -> VfsResult<StatFs> {
        statfs(self.mount_flags)
    }
}

impl DirNodeOps for MockDirNodeOps {
    fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        Ok(0)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        if name != "mnt" {
            return Err(VfsError::NotFound);
        }

        Ok(DirEntry::new_dir(
            |_| {
                DirNode::new(Arc::new(MockDirNodeOps::new(
                    self.mount_flags,
                    self.inode + 1,
                )))
            },
            Reference::new(None, String::from(name)),
        ))
    }

    fn create(
        &self,
        _name: &str,
        _node_type: NodeType,
        _permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotSupported)
    }

    fn unlink(&self, _name: &str) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
}

fn mock_filesystem(mount_flags: u32) -> Filesystem {
    Filesystem::new(Arc::new(MockFilesystem { mount_flags }))
}

fn statfs(mount_flags: u32) -> VfsResult<StatFs> {
    Ok(StatFs {
        fs_type: 0,
        block_size: 0,
        blocks: 0,
        blocks_free: 0,
        blocks_available: 0,
        file_count: 0,
        free_file_count: 0,
        name_length: 255,
        fragment_size: 0,
        mount_flags,
    })
}

#[def_test]
fn test_mountpoint_thread_safety() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mountpoint>();
}

#[def_test]
fn test_root_mount_defaults_to_writable() {
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root(&fs);
    let root = mount.root_location();

    assert_eq!(mount.flags(), MountFlags::empty());
    assert!(!mount.is_readonly());
    assert!(!root.is_mount_readonly());
    assert!(!root.is_effectively_readonly());
    assert_eq!(root.check_writable_mount(), Ok(()));
}

#[def_test]
fn test_root_mount_can_be_readonly() {
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_location();

    assert!(mount.flags().contains(MountFlags::RDONLY));
    assert!(mount.is_readonly());
    assert!(root.is_mount_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_child_mount_flags_are_independent_from_parent() {
    let root_fs = mock_filesystem(0);
    let child_fs = mock_filesystem(0);
    let root_mount = Mountpoint::new_root_with_flags(&root_fs, MountFlags::RDONLY);
    let mount_dir = root_mount.root_location().lookup_no_follow("mnt").unwrap();
    let child_mount = mount_dir
        .mount_with_flags(&child_fs, MountFlags::empty())
        .unwrap();
    let child_root = child_mount.root_location();

    assert!(root_mount.is_readonly());
    assert!(!child_mount.is_readonly());
    assert!(!child_root.is_mount_readonly());
    assert!(!child_root.is_effectively_readonly());
}

#[def_test]
fn test_filesystem_stat_readonly_makes_location_effectively_readonly() {
    let fs = mock_filesystem(ST_RDONLY);
    let mount = Mountpoint::new_root(&fs);
    let root = mount.root_location();

    assert!(!mount.is_readonly());
    assert!(!root.is_mount_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

#[def_test]
fn test_mount_flags_combine() {
    let flags =
        MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NOSYMFOLLOW;
    assert!(flags.contains(MountFlags::RDONLY));
    assert!(flags.contains(MountFlags::NOSUID));
    assert!(flags.contains(MountFlags::NOEXEC));
    assert!(flags.contains(MountFlags::NOSYMFOLLOW));
    assert!(!flags.contains(MountFlags::NODEV));
    assert!(!flags.contains(MountFlags::NOATIME));
}

#[def_test]
fn test_mount_flags_relatime() {
    // Verify RELATIME is set and independent from RDONLY.
    let flags = MountFlags::RELATIME | MountFlags::NOSYMFOLLOW;
    assert!(flags.contains(MountFlags::RELATIME));
    assert!(!flags.contains(MountFlags::RDONLY));
    assert!(flags.contains(MountFlags::NOSYMFOLLOW));
}

#[def_test]
fn test_overmount_hides_previous_and_restores_on_unmount() {
    let root_fs = mock_filesystem(0);
    let fs_a = mock_filesystem(0);
    let fs_b = mock_filesystem(0);

    let root_mount = Mountpoint::new_root(&root_fs);
    let mnt_loc = root_mount.root_location().lookup_no_follow("mnt").unwrap();

    // Mount A at mnt
    let mount_a = mnt_loc.mount(&fs_a).unwrap();
    let loc_a = root_mount.root_location().lookup_no_follow("mnt").unwrap();
    assert!(Arc::ptr_eq(&loc_a.mountpoint().clone(), &mount_a));

    // Mount B at same location — overmount should succeed
    let mount_b = mnt_loc.mount_with_flags(&fs_b, MountFlags::RDONLY).unwrap();
    let loc_b = root_mount.root_location().lookup_no_follow("mnt").unwrap();
    assert!(Arc::ptr_eq(&loc_b.mountpoint().clone(), &mount_b));
    assert!(loc_b.is_mount_readonly());

    // Unmount B — A should be visible again
    mount_b.root_location().unmount().unwrap();
    let loc_a_again = root_mount.root_location().lookup_no_follow("mnt").unwrap();
    assert!(Arc::ptr_eq(&loc_a_again.mountpoint().clone(), &mount_a));
}

#[def_test]
fn test_readonly_mount_blocks_create() {
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_location();

    let result = root.create("test", NodeType::RegularFile, NodePermission::default());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), VfsError::ReadOnlyFilesystem);
}

#[def_test]
fn test_readonly_mount_blocks_unlink() {
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_location();

    let result = root.unlink("test", false);
    assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
}

#[def_test]
fn test_readonly_mount_blocks_rename() {
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_location();

    let result = root.rename("a", &root, "b");
    assert_eq!(result, Err(VfsError::ReadOnlyFilesystem));
}

#[def_test]
fn test_writable_mount_allows_operations_to_reach_filesystem() {
    // On a writable mount, the check passes and we reach the mock FS
    // which returns its own error (not ReadOnlyFilesystem).
    let fs = mock_filesystem(0);
    let mount = Mountpoint::new_root(&fs);
    let root = mount.root_location();

    let result = root.create("test", NodeType::RegularFile, NodePermission::default());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), VfsError::OperationNotSupported);

    // unlink looks up the entry first; the mock only has "mnt",
    // so we get NotFound — crucially NOT ReadOnlyFilesystem.
    assert_eq!(root.unlink("test", false), Err(VfsError::NotFound));
}

#[def_test]
fn test_stat_rdonly_combined_with_mount_rdonly_is_readonly() {
    let fs = mock_filesystem(ST_RDONLY);
    let mount = Mountpoint::new_root_with_flags(&fs, MountFlags::RDONLY);
    let root = mount.root_location();

    assert!(mount.is_readonly());
    assert!(root.is_effectively_readonly());
    assert_eq!(
        root.check_writable_mount(),
        Err(VfsError::ReadOnlyFilesystem)
    );
}

// Systematic cross-check: every ST_* and MountFlags constant must match
// the expected value.  This catches copy-paste mistakes.

#[def_test]
fn test_st_constants_values() {
    assert_eq!(ST_RDONLY, 0x0001);
    assert_eq!(ST_NOSUID, 0x0002);
    assert_eq!(ST_NODEV, 0x0004);
    assert_eq!(ST_NOEXEC, 0x0008);
    assert_eq!(ST_NOATIME, 0x0400);
    assert_eq!(ST_NODIRATIME, 0x0800);
    assert_eq!(ST_RELATIME, 0x1000);
    assert_eq!(ST_NOSYMFOLLOW, 0x2000);
}

#[def_test]
fn test_mount_flags_constants_values() {
    assert_eq!(MountFlags::NOSUID.bits(), 0x01);
    assert_eq!(MountFlags::NODEV.bits(), 0x02);
    assert_eq!(MountFlags::NOEXEC.bits(), 0x04);
    assert_eq!(MountFlags::NOATIME.bits(), 0x08);
    assert_eq!(MountFlags::NODIRATIME.bits(), 0x10);
    assert_eq!(MountFlags::RELATIME.bits(), 0x20);
    assert_eq!(MountFlags::RDONLY.bits(), 0x40);
    assert_eq!(MountFlags::NOSYMFOLLOW.bits(), 0x80);
}
