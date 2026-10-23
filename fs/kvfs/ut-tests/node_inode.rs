// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};

use ktime_types::{NANOS_PER_SEC, SystemTime};
use unittest::{assert_eq, def_test};

use super::*;
use crate::{
    GetattrQueryFlags, GetattrRequestMask, NodePermission, StatFs, SuperBlockFlags,
    SuperBlockOperations, TimestampLimits,
};

struct TestChrdevInode {
    metadata: Mutex<Metadata>,
    last_update: Mutex<Option<MetadataUpdate>>,
    next_applied_update: Mutex<Option<MetadataUpdate>>,
}

impl TestChrdevInode {
    fn new(rdev: DeviceId) -> Self {
        Self {
            metadata: Mutex::new(Metadata {
                device: 0,
                inode: 42,
                nlink: 1,
                mode: Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 0,
                blocks: 0,
                rdev,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
            }),
            last_update: Mutex::new(None),
            next_applied_update: Mutex::new(None),
        }
    }
}

impl InodeOperations for TestChrdevInode {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(self.metadata.lock().clone())
    }

    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        *self.last_update.lock() = Some(update.clone());
        Ok(self.next_applied_update.lock().take().unwrap_or(update))
    }
}

impl FileOperations for TestChrdevInode {}

impl AddressSpaceOperations for TestChrdevInode {}

impl InodeAttributeOperations for TestChrdevInode {
    fn fill_metadata(&self, inode_number: u64, block_size: u64) -> Metadata {
        let mut metadata = self.metadata.lock().clone();
        metadata.inode = inode_number;
        metadata.block_size = block_size;
        metadata
    }

    fn times(&self) -> InodeTimes {
        let metadata = self.metadata.lock();
        InodeTimes {
            accessed_at: metadata.atime,
            modified_at: metadata.mtime,
            changed_at: metadata.ctime,
        }
    }

    fn mode(&self) -> Umode {
        self.metadata.lock().mode
    }

    fn owner(&self) -> (u32, u32) {
        let metadata = self.metadata.lock();
        (metadata.uid, metadata.gid)
    }

    fn link_count(&self) -> u64 {
        self.metadata.lock().nlink
    }

    fn generation(&self) -> u32 {
        17
    }

    fn rdev(&self) -> DeviceId {
        self.metadata.lock().rdev
    }

    fn size(&self) -> u64 {
        self.metadata.lock().size
    }

    fn blocks(&self) -> u64 {
        self.metadata.lock().blocks
    }

    fn set_permission(&self, permission: NodePermission) {
        let mut metadata = self.metadata.lock();
        metadata.mode = Umode::new(metadata.mode.node_type(), permission);
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        let mut metadata = self.metadata.lock();
        metadata.uid = uid;
        metadata.gid = gid;
    }

    fn set_link_count(&self, link_count: u64) {
        self.metadata.lock().nlink = link_count;
    }

    fn increment_link_count(&self) {
        let mut metadata = self.metadata.lock();
        metadata.nlink = metadata.nlink.saturating_add(1);
    }

    fn decrement_link_count(&self) {
        let mut metadata = self.metadata.lock();
        metadata.nlink = metadata.nlink.saturating_sub(1);
    }

    fn set_size(&self, size: u64) {
        self.metadata.lock().size = size;
    }

    fn set_accessed_at(&self, value: SystemTime) {
        self.metadata.lock().atime = value;
    }

    fn set_modified_at(&self, value: SystemTime) {
        self.metadata.lock().mtime = value;
    }

    fn set_changed_at(&self, value: SystemTime) {
        self.metadata.lock().ctime = value;
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.metadata.lock().blocks = bytes / INODE_BYTES_PER_BLOCK;
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        let mut metadata = self.metadata.lock();
        metadata.blocks = metadata
            .blocks
            .saturating_add(bytes / INODE_BYTES_PER_BLOCK);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        let mut metadata = self.metadata.lock();
        metadata.blocks = metadata
            .blocks
            .saturating_sub(bytes / INODE_BYTES_PER_BLOCK);
    }
}

struct SecondResolutionSuperBlockOperations;

static SECOND_RESOLUTION_SUPER_BLOCK_OPERATIONS: SecondResolutionSuperBlockOperations =
    SecondResolutionSuperBlockOperations;
static SECOND_RESOLUTION_FILE_SYSTEM_TYPE: crate::FileSystemType =
    crate::FileSystemType::internal("timestamp-limits-test");

impl SuperBlockOperations for SecondResolutionSuperBlockOperations {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0,
            block_size: 512,
            blocks: 1,
            blocks_free: 1,
            blocks_available: 1,
            file_count: 1,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 512,
        })
    }

    fn timestamp_limits(&self, _super_block: &SuperBlock) -> TimestampLimits {
        TimestampLimits::new(NANOS_PER_SEC as u32, i32::MIN as i64, i32::MAX as i64)
    }
}

fn second_resolution_super_block() -> Arc<SuperBlock> {
    SuperBlock::new_with_flags_and_private(
        &SECOND_RESOLUTION_FILE_SYSTEM_TYPE,
        &SECOND_RESOLUTION_SUPER_BLOCK_OPERATIONS,
        (),
        SuperBlockFlags::empty(),
        1,
        crate::MAX_LFS_FILESIZE,
        |_| {
            Dentry::new_dir_from_inode(
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        1,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                ),
                None,
                String::new(),
            )
        },
    )
}

#[def_test]
fn special_inodes_preserve_rdev() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
    assert_eq!(inode.metadata().inode, 7);
    assert_eq!(inode.metadata().rdev, DeviceId::new(1, 3));
    assert_eq!(inode.grow_size_after_backing_write(16), Ok(()));
    assert_eq!(inode.size(), 0);
}

#[def_test]
fn filesystem_attribute_operations_are_the_vfs_inode_state() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let init = VfsInodeInit::new(
        42,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_with_inode_attribute_operations(
        node.clone(),
        NodeFlags::NON_CACHEABLE,
        empty_address_space_operations(),
        init,
    );

    inode.set_link_count(5);
    inode.set_size(23);
    assert_eq!(node.metadata.lock().nlink, 5);
    assert_eq!(node.metadata.lock().size, 23);

    node.metadata.lock().uid = 1000;
    node.metadata.lock().size = 41;
    assert_eq!(inode.metadata().uid, 1000);
    assert_eq!(inode.size(), 41);
    assert_eq!(inode.generation(), 17);
}

#[def_test]
fn inode_time_updates_use_superblock_timestamp_limits() {
    let super_block = second_resolution_super_block();
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let inode = super_block.get_or_init_inode(42, || {
        VfsInode::new_with_inode_attribute_operations(
            node.clone(),
            NodeFlags::NON_CACHEABLE,
            empty_address_space_operations(),
            VfsInodeInit::new(
                42,
                0,
                Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
            )
            .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3)),
        )
    });
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, String::from("timestamp"));

    dentry
        .update_metadata(MetadataUpdate {
            atime: Some(SystemTime::from_unix_parts(11, 111).unwrap()),
            mtime: Some(SystemTime::from_unix_parts(12, 222).unwrap()),
            ctime: Some(SystemTime::from_unix_parts(13, 333).unwrap()),
            ..Default::default()
        })
        .unwrap();

    let callback_update = node.last_update.lock().clone().unwrap();
    assert_eq!(
        callback_update.atime,
        Some(SystemTime::from_unix_seconds(11))
    );
    assert_eq!(
        callback_update.mtime,
        Some(SystemTime::from_unix_seconds(12))
    );
    assert_eq!(
        callback_update.ctime,
        Some(SystemTime::from_unix_seconds(13))
    );
    assert_eq!(inode.metadata().atime, SystemTime::from_unix_seconds(11));
    assert_eq!(inode.metadata().mtime, SystemTime::from_unix_seconds(12));
    assert_eq!(inode.metadata().ctime, SystemTime::from_unix_seconds(13));

    inode
        .update_time(
            &dentry,
            inode.truncate_timestamp(SystemTime::from_unix_parts(14, 444).unwrap()),
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();
    let callback_update = node.last_update.lock().clone().unwrap();
    assert_eq!(
        callback_update.mtime,
        Some(SystemTime::from_unix_seconds(14))
    );
    assert_eq!(
        callback_update.ctime,
        Some(SystemTime::from_unix_seconds(14))
    );
    assert_eq!(inode.metadata().mtime, SystemTime::from_unix_seconds(14));
    assert_eq!(inode.metadata().ctime, SystemTime::from_unix_seconds(14));

    assert_eq!(
        inode.truncate_timestamp(SystemTime::from_unix_parts(15, 555).unwrap()),
        SystemTime::from_unix_seconds(15)
    );
    assert_eq!(inode.current_time().subsec_nanos(), 0);
    assert_eq!(
        inode.set_accessed_at(SystemTime::from_unix_parts(15, 555).unwrap()),
        SystemTime::from_unix_parts(15, 555).unwrap()
    );
    assert_eq!(
        inode.metadata().atime,
        SystemTime::from_unix_parts(15, 555).unwrap()
    );
}

#[def_test]
fn resident_timestamps_follow_values_returned_by_filesystem() {
    let super_block = second_resolution_super_block();
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let inode = super_block.get_or_init_inode(43, || {
        VfsInode::new_with_inode_attribute_operations(
            node.clone(),
            NodeFlags::NON_CACHEABLE,
            empty_address_space_operations(),
            VfsInodeInit::new(
                43,
                0,
                Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
            ),
        )
    });
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, String::from("applied"));
    let applied_atime = SystemTime::from_unix_seconds(20);
    *node.next_applied_update.lock() = Some(MetadataUpdate {
        atime: Some(applied_atime),
        ..Default::default()
    });

    dentry
        .update_metadata(MetadataUpdate {
            atime: Some(SystemTime::from_unix_seconds(21)),
            mtime: Some(SystemTime::from_unix_seconds(22)),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(inode.metadata().atime, applied_atime);
    assert_eq!(inode.metadata().mtime, SystemTime::UNIX_EPOCH);

    let applied_cmtime = SystemTime::from_unix_seconds(24);
    *node.next_applied_update.lock() = Some(MetadataUpdate {
        mtime: Some(applied_cmtime),
        ctime: Some(applied_cmtime),
        ..Default::default()
    });
    inode
        .update_time(
            &dentry,
            SystemTime::from_unix_seconds(25),
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();

    assert_eq!(inode.metadata().mtime, applied_cmtime);
    assert_eq!(inode.metadata().ctime, applied_cmtime);
}

#[def_test]
fn inode_link_count_helpers_update_link_count() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);

    assert_eq!(inode.link_count(), 1);
    inode.increment_link_count();
    assert_eq!(inode.link_count(), 2);
    inode.decrement_link_count();
    assert_eq!(inode.link_count(), 1);
    inode.increment_link_count();
    assert_eq!(inode.link_count(), 2);
    inode.decrement_link_count();
    assert_eq!(inode.link_count(), 1);
    inode.set_link_count(7);
    assert_eq!(inode.link_count(), 7);
    inode.clear_link_count();
    assert_eq!(inode.link_count(), 0);
}

#[def_test]
fn backing_metadata_refresh_updates_mutable_cached_attributes() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
    let mut updated = inode.metadata();
    updated.nlink = 0;
    updated.mode = updated
        .mode
        .with_permission(NodePermission::from_bits_truncate(0o640));
    updated.uid = 1000;
    updated.gid = 1001;
    updated.blocks = 7;
    updated.atime = SystemTime::from_unix_parts(11, 1).unwrap();
    updated.mtime = SystemTime::from_unix_parts(12, 2).unwrap();
    updated.ctime = SystemTime::from_unix_parts(13, 3).unwrap();

    inode
        .update_attributes_after_backing_change(&updated)
        .unwrap();

    let cached = inode.metadata();
    assert_eq!(cached.nlink, 0);
    assert_eq!(cached.mode.permission().bits(), 0o640);
    assert_eq!((cached.uid, cached.gid), (1000, 1001));
    assert_eq!(cached.blocks, 7);
    assert_eq!(cached.atime, SystemTime::from_unix_parts(11, 1).unwrap());
    assert_eq!(cached.mtime, SystemTime::from_unix_parts(12, 2).unwrap());
    assert_eq!(cached.ctime, SystemTime::from_unix_parts(13, 3).unwrap());

    updated.inode = 8;
    assert_eq!(
        inode.update_attributes_after_backing_change(&updated),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn metadata_only_inode_operations_accepts_owner_changes() {
    let ops = metadata_only_inode_operations();
    let inode = VfsInode::new_dir(
        Arc::new(EmptyInodeOperations),
        VfsInodeInit::new(
            1,
            0,
            Umode::new(NodeType::Directory, NodePermission::empty()),
        ),
    );
    let dentry = Dentry::new_dir_from_inode(inode, None, String::new());
    let update = MetadataUpdate {
        owner: Some((1, 2)),
        mode: Some(NodePermission::from_bits_truncate(0o600)),
        ..Default::default()
    };
    let applied = ops
        .setattr(&MountIdmap, &dentry, update.clone())
        .expect("metadata-only ops must accept setattr");
    assert_eq!(applied.owner, Some((1, 2)));
    assert_eq!(
        applied.mode.map(|mode| mode.bits()),
        Some(NodePermission::from_bits_truncate(0o600).bits())
    );
}

#[def_test]
fn file_lock_context_allocation_failure_is_retryable() {
    let inode = VfsInode::new_dir(
        Arc::new(EmptyInodeOperations),
        VfsInodeInit::new(
            1,
            0,
            Umode::new(NodeType::Directory, NodePermission::empty()),
        ),
    );
    let result = inode.init_file_locks(|| Err(VfsError::NoMemory));
    assert_eq!(result.err(), Some(VfsError::NoMemory));
    assert!(inode.existing_file_locks().is_none());
    let context = inode.file_locks().unwrap();
    let reused = inode.init_file_locks(|| Err(VfsError::NoMemory)).unwrap();
    assert!(core::ptr::eq(context, reused));
    assert!(core::ptr::eq(context, inode.existing_file_locks().unwrap()));
}
