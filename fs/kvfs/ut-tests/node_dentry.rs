// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use ktime_types::SystemTime;
use memaddr::PAGE_SIZE_4K;
use pagecache::Folio;
use unittest::{assert, def_test};

use super::{Dentry, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive};
use crate::{
    AddressSpace, AddressSpaceOperations, DirContext, FileDirOperations, FileOperations,
    InodeDirOperations, InodeOperations, InodeSymlinkOperations, LockedDentry, Metadata,
    MetadataUpdate, NodeFlags, NodePermission, NodeType, RenameFlags, StatFs, SuperBlock,
    SuperBlockOperations, Umode, VfsError, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    WritebackControl,
};

struct MockFilesystem;

static MOCK_SUPER_OPERATIONS: MockFilesystem = MockFilesystem;
static MOCK_FILE_SYSTEM_TYPE: crate::FileSystemType = crate::FileSystemType::internal("mockfs");

impl SuperBlockOperations for MockFilesystem {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
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
        })
    }
}

struct MockFileOps {
    inode: u64,
    node_type: NodeType,
    data: crate::Mutex<Vec<u8>>,
    owner: crate::Mutex<Option<(u32, u32)>>,
    update_count: AtomicUsize,
}

impl MockFileOps {
    fn new(_fs: Arc<MockFilesystem>, inode: u64, data: &[u8]) -> Self {
        Self::new_with_type(inode, NodeType::RegularFile, data)
    }

    fn new_with_type(inode: u64, node_type: NodeType, data: &[u8]) -> Self {
        Self {
            inode,
            node_type,
            data: crate::Mutex::new(data.to_vec()),
            owner: crate::Mutex::new(None),
            update_count: AtomicUsize::new(0),
        }
    }
}

impl InodeOperations for MockFileOps {
    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        if self.node_type == NodeType::Symlink {
            Some(self)
        } else {
            None
        }
    }

    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: crate::Umode::new(self.node_type, NodePermission::default()),
            uid: 0,
            gid: 0,
            size: self.data.lock().len() as u64,
            block_size: 512,
            blocks: 1,
            rdev: Default::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        })
    }

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        *self.owner.lock() = update.owner;
        self.update_count.fetch_add(1, Ordering::Relaxed);
        Ok(update)
    }
}

impl InodeSymlinkOperations for MockFileOps {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &VfsInode,
        _done: &mut crate::DelayedCall,
    ) -> VfsResult<String> {
        String::from_utf8(self.data.lock().clone()).map_err(|_| VfsError::InvalidData)
    }
}

impl FileOperations for MockFileOps {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.data.lock();
        let start = offset as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let count = buf.len().min(data.len() - start);
        buf[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut data = self.data.lock();
        let start = offset as usize;
        if start + buf.len() > data.len() {
            data.resize(start + buf.len(), 0);
        }
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }
}

struct MockAddressSpaceOperations;

static MOCK_ADDRESS_SPACE_OPERATIONS: MockAddressSpaceOperations = MockAddressSpaceOperations;

impl AddressSpaceOperations for MockAddressSpaceOperations {
    fn read_folio(&self, mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<()> {
        let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let data = inode.private::<MockFileOps>()?.data.lock();
        let start = folio
            .index()
            .checked_mul(PAGE_SIZE_4K as u64)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(VfsError::InvalidInput)?;
        if start >= data.len() {
            folio.data().fill(0);
            return Ok(());
        }
        let buf = folio.data();
        let count = buf.len().min(data.len() - start);
        buf[..count].copy_from_slice(&data[start..start + count]);
        buf[count..].fill(0);
        Ok(())
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
        let backing = inode.private::<MockFileOps>()?;
        mapping.writeback_cached_folios(control, |index, buf, valid_len| {
            let start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or(VfsError::InvalidInput)?;
            let end = start.checked_add(valid_len).ok_or(VfsError::InvalidInput)?;
            let mut data = backing.data.lock();
            if end > data.len() {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(&buf[..valid_len]);
            Ok(())
        })
    }
}

struct MockDirOps {
    inode: u64,
    can_rename: bool,
    can_remove: bool,
    unlink_count: AtomicUsize,
    rmdir_count: AtomicUsize,
    lookup_result: crate::Mutex<Option<Dentry>>,
}

impl MockDirOps {
    fn new(_fs: Arc<MockFilesystem>, inode: u64) -> Self {
        Self {
            inode,
            can_rename: false,
            can_remove: false,
            unlink_count: AtomicUsize::new(0),
            rmdir_count: AtomicUsize::new(0),
            lookup_result: crate::Mutex::new(None),
        }
    }

    fn new_renamable(inode: u64) -> Self {
        Self {
            inode,
            can_rename: true,
            can_remove: false,
            unlink_count: AtomicUsize::new(0),
            rmdir_count: AtomicUsize::new(0),
            lookup_result: crate::Mutex::new(None),
        }
    }

    fn new_removable(inode: u64) -> Self {
        Self {
            inode,
            can_rename: false,
            can_remove: true,
            unlink_count: AtomicUsize::new(0),
            rmdir_count: AtomicUsize::new(0),
            lookup_result: crate::Mutex::new(None),
        }
    }
}

impl InodeOperations for MockDirOps {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        _idmap: &crate::MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: crate::GetattrRequestMask,
        _query_flags: crate::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: crate::Umode::new(NodeType::Directory, NodePermission::default()),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 512,
            blocks: 1,
            rdev: Default::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        })
    }

    fn setattr(
        &self,
        _idmap: &crate::MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        Ok(update)
    }
}

impl InodeDirOperations for MockDirOps {
    fn lookup(
        &self,
        _dir: &VfsInode,
        _dentry: &LockedDentry<'_>,
        _flags: crate::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        Ok(self.lookup_result.lock().clone())
    }

    fn create(
        &self,
        _idmap: &crate::MountIdmap,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: crate::Umode,
        _exclusive: bool,
        _cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let operations = Arc::new(MockFileOps::new(
            Arc::new(MockFilesystem),
            self.inode + 1,
            &[],
        ));
        let inode = VfsInode::new_file(operations, VfsInodeInit::new(self.inode + 1, 0, mode));
        dentry.instantiate(inode)
    }

    fn link(
        &self,
        _old_dentry: &Dentry,
        _dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
    ) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }

    fn unlink(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.unlink_count.fetch_add(1, Ordering::Relaxed);
        if self.can_remove {
            Ok(())
        } else {
            Err(VfsError::OperationNotSupported)
        }
    }

    fn rmdir(&self, _dir: &VfsInode, _dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.rmdir_count.fetch_add(1, Ordering::Relaxed);
        if self.can_remove {
            Ok(())
        } else {
            Err(VfsError::OperationNotSupported)
        }
    }

    fn rename(
        &self,
        _idmap: &crate::MountIdmap,
        _old_dir: &VfsInode,
        _old_dentry: &LockedDentry<'_>,
        _new_dir: &VfsInode,
        _new_dentry: &LockedDentry<'_>,
        _flags: crate::RenameFlags,
    ) -> VfsResult<()> {
        if self.can_rename {
            Ok(())
        } else {
            Err(VfsError::OperationNotSupported)
        }
    }
}

impl FileOperations for MockDirOps {
    fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
        Some(self)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
}

impl FileDirOperations for MockDirOps {
    fn iterate_shared(
        &self,
        _file: &crate::VfsFile,
        _ctx: &mut DirContext<'_>,
    ) -> VfsResult<usize> {
        Ok(0)
    }
}

fn make_file_entry(
    fs: Arc<MockFilesystem>,
    inode: u64,
    parent: Option<Dentry>,
    name: &str,
) -> (Dentry, Arc<MockFileOps>) {
    let ops = Arc::new(MockFileOps::new(fs, inode, b"payload"));
    let inode = VfsInode::new_file(
        ops.clone(),
        inode_init(inode, NodeType::RegularFile, b"payload".len() as u64),
    );
    let entry = Dentry::new_file_from_inode(inode, parent, String::from(name));
    (entry, ops)
}

fn make_dir_entry(fs: Arc<MockFilesystem>, inode: u64, name: &str) -> Dentry {
    make_dir_entry_with_parent(fs, inode, None, name)
}

fn make_dir_entry_with_parent(
    fs: Arc<MockFilesystem>,
    inode: u64,
    parent: Option<Dentry>,
    name: &str,
) -> Dentry {
    let inode = VfsInode::new_openable_dir(
        Arc::new(MockDirOps::new(fs, inode)),
        inode_init(inode, NodeType::Directory, 0),
    );
    Dentry::new_dir_from_inode(inode, parent, String::from(name))
}

fn make_renamable_dir_entry(inode: u64, parent: Option<Dentry>, name: &str) -> Dentry {
    let inode = VfsInode::new_openable_dir(
        Arc::new(MockDirOps::new_renamable(inode)),
        inode_init(inode, NodeType::Directory, 0),
    );
    Dentry::new_dir_from_inode(inode, parent, String::from(name))
}

fn make_removable_dir_entry(
    inode: u64,
    parent: Option<Dentry>,
    name: &str,
) -> (Dentry, Arc<MockDirOps>) {
    let operations = Arc::new(MockDirOps::new_removable(inode));
    let inode = VfsInode::new_openable_dir(
        operations.clone(),
        inode_init(inode, NodeType::Directory, 0),
    );
    (
        Dentry::new_dir_from_inode(inode, parent, String::from(name)),
        operations,
    )
}

fn inode_init(inode: u64, node_type: NodeType, size: u64) -> VfsInodeInit {
    VfsInodeInit::new(
        inode,
        size,
        crate::Umode::new(node_type, NodePermission::default()),
    )
    .with_owner_links_and_rdev(0, 0, 1, Default::default())
    .with_stat_data(
        4096,
        1,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
        SystemTime::UNIX_EPOCH,
    )
}

#[def_test]
fn test_dentry_file_helpers_and_private_data() {
    let fs = Arc::new(MockFilesystem);
    let (entry, ops) = make_file_entry(fs, 2, None, "leaf");

    assert!(entry.is_file());
    assert!(!entry.is_dir());
    assert_eq!(entry.name_snapshot(), "leaf");
    assert_eq!(entry.node_type(), NodeType::RegularFile);
    assert!(matches!(entry.as_dir(), Err(VfsError::NotADirectory)));
    assert_eq!(entry.inode(), 2);

    let metadata = d_inode(&entry).metadata();
    assert_eq!(metadata.mode.node_type(), NodeType::RegularFile);
    assert_eq!(metadata.size, 7);

    assert_eq!(ops.update_count.load(Ordering::Relaxed), 0);
}

#[def_test]
fn test_dentry_backing_metadata_refresh_validates_entry_state() {
    let fs = Arc::new(MockFilesystem);
    let (entry, _) = make_file_entry(fs, 2, None, "leaf");
    let mut updated = entry.metadata();
    updated.nlink = 2;
    updated.size = 11;

    entry
        .update_metadata_after_backing_change(&updated)
        .unwrap();
    assert_eq!(entry.metadata().nlink, 2);
    assert_eq!(entry.metadata().size, 11);

    updated.inode = 3;
    assert_eq!(
        entry.update_metadata_after_backing_change(&updated),
        Err(VfsError::InvalidInput)
    );

    let negative = Dentry::new_negative(None, String::from("missing"));
    assert_eq!(
        negative.update_metadata_after_backing_change(&updated),
        Err(VfsError::NotFound)
    );
}

#[def_test]
fn test_dentry_positive_and_negative_helpers() {
    let negative = Dentry::new_negative(None, String::from("missing"));
    assert!(d_is_negative(&negative));
    assert!(!negative.is_positive());
    assert!(!d_is_dir(&negative));
    assert!(!negative.is_regular_entry());
    assert!(!negative.is_special_entry());
    assert!(!d_is_symlink(&negative));
    assert!(negative.is_really_negative());
    assert!(!d_really_is_positive(&negative));

    let _fs = Arc::new(MockFilesystem);
    let unknown_inode = VfsInode::new_file(
        Arc::new(MockFileOps::new_with_type(
            23,
            NodeType::Unknown,
            b"unknown",
        )),
        inode_init(23, NodeType::Unknown, b"unknown".len() as u64),
    );
    let unknown = Dentry::new_file_from_inode(unknown_inode, None, String::from("unknown"));
    assert!(!d_is_negative(&unknown));
    assert!(unknown.is_positive());
    assert!(!d_is_dir(&unknown));
    assert!(!unknown.is_regular_entry());
    assert!(unknown.is_special_entry());
    assert!(unknown.is_file_entry());
    assert!(!d_is_symlink(&unknown));
    assert!(!unknown.is_really_negative());
    assert!(d_really_is_positive(&unknown));
}

#[def_test]
fn test_invalidate_cached_child_handles_negative_dentry() {
    let fs = Arc::new(MockFilesystem);
    let root = make_dir_entry(fs, 20, "");
    let negative = Dentry::new_negative(Some(root.clone()), String::from("missing"));

    root.insert_cache(negative.name_snapshot(), negative.clone());
    assert!(root.lookup_cache("missing").unwrap().ptr_eq(&negative));

    root.invalidate_cached_child("missing").unwrap();
    assert!(root.lookup_cache("missing").is_none());
}

#[def_test]
fn test_create_instantiates_cached_lookup_miss() {
    let fs = Arc::new(MockFilesystem);
    let root = make_dir_entry(fs.clone(), 24, "");
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });

    assert!(matches!(root.lookup("created"), Err(VfsError::NotFound)));
    let negative = root.lookup_cache("created").unwrap();
    let created = root
        .create_exclusive_with("created", |candidate| {
            root.vfs_inode().create_with_mode(
                candidate,
                Umode::new(NodeType::RegularFile, NodePermission::default()),
                true,
                &kcred::initial_cred(),
            )
        })
        .unwrap();

    assert!(created.ptr_eq(&negative));
    assert!(created.is_really_positive());
    assert!(root.lookup_cache("created").unwrap().ptr_eq(&created));
}

#[def_test]
fn test_rename_moves_cache_entry_without_changing_inode_identity() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(30, None, "");
    let (source, _) = make_file_entry(fs, 31, Some(root.clone()), "tmp");

    root.insert_cache(String::from("tmp"), source.clone());
    root.rename("tmp", &root, "final", RenameFlags::empty())
        .unwrap();

    assert!(root.lookup_cache("tmp").is_none());
    let moved = root.lookup_cache("final").unwrap();
    assert!(moved.ptr_eq(&source));
    assert!(moved.is_same_inode(&source));
    assert_eq!(moved.name_snapshot(), "final");
    assert_eq!(moved.absolute_path().unwrap().as_str(), "/final");
}

#[def_test]
fn test_rename_replace_keeps_source_inode_identity() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(32, None, "");
    let (source, _) = make_file_entry(fs.clone(), 33, Some(root.clone()), "tmp");
    let (target, _) = make_file_entry(fs, 34, Some(root.clone()), "final");

    root.insert_cache(String::from("tmp"), source.clone());
    root.insert_cache(String::from("final"), target.clone());
    root.rename("tmp", &root, "final", RenameFlags::empty())
        .unwrap();

    let moved = root.lookup_cache("final").unwrap();
    assert!(moved.ptr_eq(&source));
    assert!(!moved.is_same_inode(&target));
}

#[def_test]
fn test_rename_noreplace_rejects_existing_target_before_filesystem_callback() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(35, None, "");
    let (source, _) = make_file_entry(fs.clone(), 36, Some(root.clone()), "tmp");
    let (target, _) = make_file_entry(fs, 37, Some(root.clone()), "final");

    root.insert_cache(String::from("tmp"), source.clone());
    root.insert_cache(String::from("final"), target.clone());

    assert_eq!(
        root.rename("tmp", &root, "final", RenameFlags::NOREPLACE),
        Err(VfsError::AlreadyExists)
    );
    assert!(root.lookup_cache("tmp").unwrap().ptr_eq(&source));
    assert!(root.lookup_cache("final").unwrap().ptr_eq(&target));
}

#[def_test]
fn test_rename_noreplace_rejects_target_with_source_inode() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(69, None, "");
    let (source, _) = make_file_entry(fs, 70, Some(root.clone()), "tmp");
    let target = Dentry::new_file_from_inode(
        source.vfs_inode(),
        Some(root.clone()),
        String::from("final"),
    );

    root.insert_cache(String::from("tmp"), source.clone());
    root.insert_cache(String::from("final"), target.clone());

    assert_eq!(
        root.rename("tmp", &root, "final", RenameFlags::NOREPLACE),
        Err(VfsError::AlreadyExists)
    );
    assert!(root.lookup_cache("tmp").unwrap().ptr_eq(&source));
    assert!(root.lookup_cache("final").unwrap().ptr_eq(&target));
}

#[def_test]
fn test_rename_exchange_swaps_cache_entries_without_changing_inode_identity() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(38, None, "");
    let (left, _) = make_file_entry(fs.clone(), 39, Some(root.clone()), "left");
    let (right, _) = make_file_entry(fs, 40, Some(root.clone()), "right");

    root.insert_cache(String::from("left"), left.clone());
    root.insert_cache(String::from("right"), right.clone());
    root.rename("left", &root, "right", RenameFlags::EXCHANGE)
        .unwrap();

    assert!(root.lookup_cache("left").unwrap().ptr_eq(&right));
    assert!(root.lookup_cache("right").unwrap().ptr_eq(&left));
    assert_eq!(left.name_snapshot(), "right");
    assert_eq!(right.name_snapshot(), "left");
}

#[def_test]
fn test_rename_rejects_noreplace_exchange_combination() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(41, None, "");
    let (left, _) = make_file_entry(fs.clone(), 42, Some(root.clone()), "left");
    let (right, _) = make_file_entry(fs, 43, Some(root.clone()), "right");

    root.insert_cache(String::from("left"), left.clone());
    root.insert_cache(String::from("right"), right.clone());

    assert_eq!(
        root.rename(
            "left",
            &root,
            "right",
            RenameFlags::NOREPLACE | RenameFlags::EXCHANGE,
        ),
        Err(VfsError::InvalidInput)
    );
    assert!(root.lookup_cache("left").unwrap().ptr_eq(&left));
    assert!(root.lookup_cache("right").unwrap().ptr_eq(&right));
}

#[def_test]
fn test_rmdir_uses_directory_removal_callback() {
    let root_operations = Arc::new(MockDirOps::new_removable(44));
    let root_inode = VfsInode::new_openable_dir(
        root_operations.clone(),
        inode_init(44, NodeType::Directory, 0),
    );
    let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
    let (victim, _) = make_removable_dir_entry(45, Some(root.clone()), "victim");
    let (cached_child, _) =
        make_file_entry(Arc::new(MockFilesystem), 46, Some(victim.clone()), "cached");
    victim.insert_cache(String::from("cached"), cached_child.clone());
    root.insert_cache(String::from("victim"), victim.clone());

    root.rmdir("victim").unwrap();

    assert_eq!(root_operations.unlink_count.load(Ordering::Relaxed), 0);
    assert_eq!(root_operations.rmdir_count.load(Ordering::Relaxed), 1);
    assert!(root.lookup_cache("victim").is_none());
}

#[def_test]
fn test_unlink_validator_observes_callback_victim() {
    let root_operations = Arc::new(MockDirOps::new_removable(60));
    let root_inode = VfsInode::new_openable_dir(
        root_operations.clone(),
        inode_init(60, NodeType::Directory, 0),
    );
    let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
    let (victim, _) = make_file_entry(Arc::new(MockFilesystem), 61, Some(root.clone()), "victim");
    root.insert_cache(String::from("victim"), victim.clone());

    assert_eq!(
        root.unlink_with("victim", |checked| {
            if !checked.ptr_eq(&victim) {
                return Err(VfsError::InvalidInput);
            }
            Err(VfsError::OperationNotPermitted)
        }),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(root_operations.unlink_count.load(Ordering::Relaxed), 0);
    assert!(root.lookup_cache("victim").unwrap().ptr_eq(&victim));
}

#[def_test]
fn test_rename_directory_emptiness_is_filesystem_owned() {
    let root = make_renamable_dir_entry(62, None, "");
    let source = make_renamable_dir_entry(63, Some(root.clone()), "source");
    let target = make_renamable_dir_entry(64, Some(root.clone()), "target");
    let (cached_child, _) =
        make_file_entry(Arc::new(MockFilesystem), 65, Some(target.clone()), "cached");
    root.insert_cache(String::from("source"), source.clone());
    root.insert_cache(String::from("target"), target.clone());
    target.insert_cache(String::from("cached"), cached_child.clone());

    root.rename("source", &root, "target", RenameFlags::empty())
        .unwrap();
    assert!(root.lookup_cache("target").unwrap().ptr_eq(&source));
}

#[def_test]
fn test_rename_rejects_directory_move_into_descendant() {
    let root = make_renamable_dir_entry(46, None, "");
    let source = make_renamable_dir_entry(47, Some(root.clone()), "source");
    let child = make_renamable_dir_entry(48, Some(source.clone()), "child");
    root.insert_cache(String::from("source"), source.clone());
    source.insert_cache(String::from("child"), child.clone());
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });

    assert_eq!(
        root.rename("source", &child, "moved", RenameFlags::empty()),
        Err(VfsError::InvalidInput)
    );
    assert!(root.lookup_cache("source").unwrap().ptr_eq(&source));
}

#[def_test]
fn test_rename_target_ancestor_uses_linux_trap_errors() {
    let root = make_renamable_dir_entry(71, None, "");
    let ancestor = make_renamable_dir_entry(72, Some(root.clone()), "ancestor");
    let old_parent = make_renamable_dir_entry(73, Some(ancestor.clone()), "old");
    let (source, _) = make_file_entry(
        Arc::new(MockFilesystem),
        74,
        Some(old_parent.clone()),
        "source",
    );
    root.insert_cache(String::from("ancestor"), ancestor.clone());
    ancestor.insert_cache(String::from("old"), old_parent.clone());
    old_parent.insert_cache(String::from("source"), source.clone());
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });

    assert_eq!(
        old_parent.rename("source", &root, "ancestor", RenameFlags::empty()),
        Err(VfsError::DirectoryNotEmpty)
    );
    assert_eq!(
        old_parent.rename("source", &root, "ancestor", RenameFlags::EXCHANGE),
        Err(VfsError::InvalidInput)
    );
    assert!(old_parent.lookup_cache("source").unwrap().ptr_eq(&source));
    assert!(root.lookup_cache("ancestor").unwrap().ptr_eq(&ancestor));
}

#[def_test]
fn test_cross_directory_rename_moves_source_dentry() {
    let fs = Arc::new(MockFilesystem);
    let root = make_renamable_dir_entry(49, None, "");
    let old_parent = make_renamable_dir_entry(50, Some(root.clone()), "old");
    let new_parent = make_renamable_dir_entry(51, Some(root.clone()), "new");
    let (source, _) = make_file_entry(fs, 52, Some(old_parent.clone()), "source");
    root.insert_cache(String::from("old"), old_parent.clone());
    root.insert_cache(String::from("new"), new_parent.clone());
    old_parent.insert_cache(String::from("source"), source.clone());
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });

    old_parent
        .rename("source", &new_parent, "target", RenameFlags::empty())
        .unwrap();

    assert!(old_parent.lookup_cache("source").is_none());
    assert!(new_parent.lookup_cache("target").unwrap().ptr_eq(&source));
    assert_eq!(source.absolute_path().unwrap().as_str(), "/new/target");
}

#[def_test]
fn test_directory_inode_rejects_second_live_alias() {
    let root = make_renamable_dir_entry(66, None, "");
    let parent_inode = VfsInode::new_openable_dir(
        Arc::new(MockDirOps::new_renamable(67)),
        inode_init(67, NodeType::Directory, 0),
    );
    let parent = Dentry::new_dir_from_inode(
        parent_inode.clone(),
        Some(root.clone()),
        String::from("parent"),
    );
    root.insert_cache(String::from("parent"), parent.clone());
    let candidate = Dentry::new_negative(Some(root), String::from("alias"));

    assert_eq!(
        candidate.instantiate(parent_inode),
        Err(VfsError::InvalidInput)
    );
    assert!(candidate.is_negative());
}

#[def_test]
fn test_directory_constructor_reuses_alias_at_same_location() {
    let root = make_renamable_dir_entry(75, None, "");
    let inode = VfsInode::new_openable_dir(
        Arc::new(MockDirOps::new_renamable(76)),
        inode_init(76, NodeType::Directory, 0),
    );
    let first =
        Dentry::new_dir_from_inode(inode.clone(), Some(root.clone()), String::from("child"));
    let second = Dentry::new_dir_from_inode(inode, Some(root), String::from("child"));

    assert!(first.ptr_eq(&second));
}

#[def_test]
fn test_remove_rejects_parent_inode_as_victim() {
    let root = make_renamable_dir_entry(77, None, "");
    root.insert_cache(String::from("self"), root.clone());

    assert_eq!(root.unlink("self"), Err(VfsError::InvalidInput));
    assert_eq!(root.rmdir("self"), Err(VfsError::InvalidInput));
}

#[def_test]
fn test_distinct_entries_create_distinct_inode_identities() {
    let fs = Arc::new(MockFilesystem);
    let (first, _) = make_file_entry(fs.clone(), 40, None, "first");
    let (second, _) = make_file_entry(fs, 40, None, "second");

    assert!(!first.ptr_eq(&second));
    assert!(!first.is_same_inode(&second));
}

#[def_test]
fn test_superblock_inode_cache_reuses_live_identity() {
    let fs = Arc::new(MockFilesystem);
    let super_block = SuperBlock::new(
        &MOCK_FILE_SYSTEM_TYPE,
        &MOCK_SUPER_OPERATIONS,
        |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        },
    );
    let failed = super_block.get_or_try_init_inode(50, || Err::<Arc<VfsInode>, _>(7u8));
    assert!(matches!(failed, Err(7)));
    let first = super_block.get_or_init_inode(50, || {
        VfsInode::new_file_with_address_space_and_flags(
            Arc::new(MockFileOps::new(fs.clone(), 50, b"first")),
            NodeFlags::empty(),
            &MOCK_ADDRESS_SPACE_OPERATIONS,
            inode_init(50, NodeType::RegularFile, b"first".len() as u64),
        )
    });
    let second = super_block.get_or_init_inode(50, || panic!("live inode should be reused"));

    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&first.super_block().unwrap(), &super_block));
    assert_eq!(first.inode(), 50);
    assert_eq!(super_block.root_dir().inode(), 1);
    assert!(super_block.lookup_inode(1).is_some());
    assert!(super_block.lookup_inode(50).is_some());

    let old_identity = Arc::downgrade(&first);
    drop(first);
    drop(second);
    assert!(old_identity.upgrade().is_none());
    assert!(super_block.lookup_inode(50).is_none());
    let replacement = super_block.get_or_init_inode(50, || {
        VfsInode::new_file_with_address_space_and_flags(
            Arc::new(MockFileOps::new(fs, 50, b"replacement")),
            NodeFlags::empty(),
            &MOCK_ADDRESS_SPACE_OPERATIONS,
            inode_init(50, NodeType::RegularFile, b"replacement".len() as u64),
        )
    });
    assert!(!core::ptr::eq(
        old_identity.as_ptr(),
        Arc::as_ptr(&replacement)
    ));
}

#[def_test]
fn test_superblock_inode_cache_isolates_equal_inode_numbers() {
    let fs = Arc::new(MockFilesystem);
    let first_super_block = SuperBlock::new(
        &MOCK_FILE_SYSTEM_TYPE,
        &MOCK_SUPER_OPERATIONS,
        |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        },
    );
    let second_super_block = SuperBlock::new(
        &MOCK_FILE_SYSTEM_TYPE,
        &MOCK_SUPER_OPERATIONS,
        |super_block| {
            let root_inode = super_block.get_or_init_inode(1, || {
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    inode_init(1, NodeType::Directory, 0),
                )
            });
            Dentry::new_dir_from_inode(root_inode, None, String::new())
        },
    );

    let first = first_super_block.get_or_init_inode(50, || {
        VfsInode::new_file_with_address_space_and_flags(
            Arc::new(MockFileOps::new(fs.clone(), 50, b"first")),
            NodeFlags::empty(),
            &MOCK_ADDRESS_SPACE_OPERATIONS,
            inode_init(50, NodeType::RegularFile, b"first".len() as u64),
        )
    });
    let second = second_super_block.get_or_init_inode(50, || {
        VfsInode::new_file_with_address_space_and_flags(
            Arc::new(MockFileOps::new(fs, 50, b"second")),
            NodeFlags::empty(),
            &MOCK_ADDRESS_SPACE_OPERATIONS,
            inode_init(50, NodeType::RegularFile, b"second".len() as u64),
        )
    });

    assert!(!Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(
        &first_super_block.lookup_inode(50).unwrap(),
        &first
    ));
    assert!(Arc::ptr_eq(
        &second_super_block.lookup_inode(50).unwrap(),
        &second
    ));
}

#[def_test]
fn test_failed_root_initialization_drops_nascent_superblock() {
    let mut nascent_super_block = None;
    let result = SuperBlock::try_new_with_flags(
        &MOCK_FILE_SYSTEM_TYPE,
        &MOCK_SUPER_OPERATIONS,
        crate::SuperBlockFlags::empty(),
        |super_block| {
            nascent_super_block = Some(Arc::downgrade(super_block));
            Err::<Dentry, _>(VfsError::Io)
        },
    );

    assert!(matches!(result, Err(VfsError::Io)));
    assert!(nascent_super_block.unwrap().upgrade().is_none());
}

#[def_test]
fn test_direntry_parent_path_and_ancestor_helpers() {
    let fs = Arc::new(MockFilesystem);
    let root = make_dir_entry(fs.clone(), 10, "");
    let child = make_dir_entry_with_parent(fs.clone(), 11, Some(root.clone()), "child");
    let (leaf, _) = make_file_entry(fs, 12, Some(child.clone()), "leaf");

    assert!(root.is_root_of_mount());
    assert_eq!(child.parent().unwrap().as_ptr(), root.as_ptr());
    assert_eq!(leaf.absolute_path().unwrap().as_str(), "/child/leaf");
    assert!(root.is_ancestor_of(&leaf).unwrap());
    assert!(child.is_ancestor_of(&leaf).unwrap());
    assert!(!leaf.is_ancestor_of(&child).unwrap());
}

#[def_test]
fn test_dentry_cache_retains_hashed_children_and_preserves_parent_lifetime() {
    let fs = Arc::new(MockFilesystem);
    let root = make_dir_entry(fs.clone(), 13, "");
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });
    let cached = make_dir_entry_with_parent(fs.clone(), 14, Some(root.clone()), "cached");
    let cached_weak = Arc::downgrade(&cached.0);
    root.insert_cache(String::from("cached"), cached.clone());

    assert!(cached_weak.upgrade().is_some());
    assert!(root.lookup_cache("cached").is_some());
    drop(cached);
    assert!(cached_weak.upgrade().is_some());
    assert!(root.lookup_cache("cached").is_some());

    root.invalidate_cached_child("cached").unwrap();
    assert!(cached_weak.upgrade().is_none());

    let parent = make_dir_entry(fs.clone(), 15, "parent");
    let parent_weak = Arc::downgrade(&parent.0);
    let child = make_dir_entry_with_parent(fs, 16, Some(parent.clone()), "child");
    drop(parent);

    assert!(parent_weak.upgrade().is_some());
    assert!(child.parent().is_some());
    assert_eq!(child.absolute_path().unwrap().as_str(), "/parent/child");
}

#[def_test]
fn test_hashed_child_without_external_reference_keeps_directory_nonempty() {
    let fs = Arc::new(MockFilesystem);
    let root = make_dir_entry(fs.clone(), 17, "");
    let _super_block = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        root.clone()
    });
    let directory = make_dir_entry_with_parent(fs.clone(), 18, Some(root.clone()), "directory");
    root.insert_cache(String::from("directory"), directory.clone());
    let (child, _) = make_file_entry(fs, 19, Some(directory.clone()), "child");
    directory.insert_cache(String::from("child"), child.clone());

    drop(child);

    assert!(directory.has_positive_children());
}

#[def_test]
fn test_direntry_read_link_and_downcast_paths() {
    let fs = Arc::new(MockFilesystem);
    let (regular, _) = make_file_entry(fs.clone(), 20, None, "plain");
    let symlink_operations = Arc::new(MockFileOps::new_with_type(
        21,
        NodeType::Symlink,
        b"/tmp/target",
    ));
    let symlink_inode = VfsInode::new_file(
        symlink_operations.clone(),
        inode_init(21, NodeType::Symlink, b"/tmp/target".len() as u64),
    );
    let symlink = Dentry::new_file_from_inode(symlink_inode, None, String::from("ln"));
    let dir = make_dir_entry(fs, 22, "dir");

    assert!(matches!(regular.read_link(), Err(VfsError::InvalidData)));
    assert_eq!(symlink.read_link().unwrap(), "/tmp/target");
    assert!(regular.downcast::<MockFileOps>().is_ok());
    assert!(matches!(
        symlink.downcast::<MockDirOps>(),
        Err(VfsError::InvalidInput)
    ));
    assert!(dir.as_dir().is_ok());
}

#[def_test]
fn test_dentry_rejects_invalid_child_names() {
    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert!(Dentry::verify_child_name(name).is_err());
    }
    assert!(Dentry::verify_child_name("file").is_ok());
}

#[def_test]
fn test_dentry_key_uniqueness() {
    let fs = Arc::new(MockFilesystem);
    let (entry1, _) = make_file_entry(fs.clone(), 1, None, "file1.txt");
    let (entry2, _) = make_file_entry(fs.clone(), 2, None, "file2.txt");
    let (entry3, _) = make_file_entry(fs, 3, None, "file1.txt");

    assert_ne!(entry1.key(), entry2.key());
    assert_eq!(entry1.key(), entry3.key());
}

fn make_non_cacheable_dir(
    fs: Arc<MockFilesystem>,
    inode: u64,
    parent: Option<Dentry>,
    name: &str,
) -> (Dentry, Arc<MockDirOps>) {
    let ops = Arc::new(MockDirOps::new(fs, inode));
    let private_data: Arc<dyn core::any::Any + Send + Sync> = ops.clone();
    let inode = VfsInode::new_dir_with_operations(
        private_data,
        ops.clone(),
        ops.clone(),
        NodeFlags::NON_CACHEABLE,
        inode_init(inode, NodeType::Directory, 0),
    );
    (
        Dentry::new_dir_from_inode(inode, parent, String::from(name)),
        ops,
    )
}

#[def_test]
fn test_lookup_non_cacheable_positive_retracts() {
    let fs = Arc::new(MockFilesystem);
    let (dir, ops) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
    let sb = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        dir.clone()
    });

    let (child, _) = make_file_entry(fs, 3, Some(dir.clone()), "x");
    *ops.lookup_result.lock() = Some(child.clone());

    let found = dir.lookup("x").unwrap();
    assert!(found.ptr_eq(&child));

    // Neither the transient candidate nor the real entry may survive in
    // the children map or the superblock dcache.
    assert!(dir.lookup_cache_entry("x").is_none());
    assert!(!sb.is_dentry_cached(&child));
}

#[def_test]
fn test_lookup_non_cacheable_negative_retracts() {
    let fs = Arc::new(MockFilesystem);
    let (dir, _) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
    let sb = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        dir.clone()
    });

    // Default MockDirOps::lookup returns Ok(None).
    assert!(matches!(dir.lookup("missing"), Err(VfsError::NotFound)));

    // The negative candidate must not survive in the children map or the
    // dcache (checked by key), and a repeated lookup must not be poisoned
    // by a stale negative dentry.
    assert!(dir.lookup_cache_entry("missing").is_none());
    let probe = Dentry::new_negative(Some(dir.clone()), String::from("missing"));
    assert!(!sb.is_dentry_cached(&probe));
    assert!(matches!(dir.lookup("missing"), Err(VfsError::NotFound)));
}

#[def_test]
fn test_bind_super_block_skips_non_cacheable_children() {
    let fs = Arc::new(MockFilesystem);
    let (dir, _) = make_non_cacheable_dir(fs.clone(), 2, None, "dir");
    let sb = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        dir.clone()
    });
    let (child, _) = make_file_entry(fs.clone(), 3, Some(dir.clone()), "child");
    dir.0
        .children
        .lock()
        .insert("child".into(), Arc::downgrade(&child.0));

    dir.bind_super_block(&sb);
    assert!(!sb.is_dentry_cached(&child));

    // Control: a cacheable directory does publish its children.
    let cacheable = make_dir_entry(fs.clone(), 4, "cacheable");
    let sb2 = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        cacheable.clone()
    });
    let (child2, _) = make_file_entry(fs, 5, Some(cacheable.clone()), "child");
    cacheable
        .0
        .children
        .lock()
        .insert("child".into(), Arc::downgrade(&child2.0));

    cacheable.bind_super_block(&sb2);
    assert!(sb2.is_dentry_cached(&child2));
}

#[def_test]
fn test_replace_lookup_candidate_preserves_replacer() {
    let fs = Arc::new(MockFilesystem);
    let dir = make_dir_entry(fs.clone(), 2, "dir");
    let sb = SuperBlock::new(&MOCK_FILE_SYSTEM_TYPE, &MOCK_SUPER_OPERATIONS, |_| {
        dir.clone()
    });

    // Simulate the concurrent-replacement window: the children slot
    // already holds another thread's candidate, so a stale replace must
    // not clobber it.
    let other = Dentry::new_negative(Some(dir.clone()), String::from("x"));
    dir.0
        .children
        .lock()
        .insert("x".into(), Arc::downgrade(&other.0));

    let candidate = Dentry::new_negative(Some(dir.clone()), String::from("x"));
    let (entry, _) = make_file_entry(fs, 3, Some(dir.clone()), "x");
    dir.replace_lookup_candidate("x", &candidate, &entry);

    // The children slot keeps the replacer; only the dcache key is
    // (defensively) refreshed with our entry.
    let cached = dir.0.children.lock().get("x").unwrap().upgrade().unwrap();
    assert!(Arc::ptr_eq(&cached, &other.0));
    assert!(sb.is_dentry_cached(&entry));
}
