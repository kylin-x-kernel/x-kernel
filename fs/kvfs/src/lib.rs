// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy kvfs has 24 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]

extern crate alloc;

mod address_space;
mod command;
mod direct_io;
mod fiemap;
mod file;
mod file_ra;
mod file_slot;
mod file_system_type;
mod filename;
mod fs_context;
mod kiocb;
pub mod libfs;
mod locks;
mod lookup;
mod mount;
mod namei;
mod node;
mod nullfs;
mod open_flags;
pub mod path;
mod permission;
pub mod pipe;
mod poll;
mod quota;
mod seq_file;
mod simple_dir;
mod simple_file;
mod simple_fs;
mod super_block;
mod transfer;
mod type_map;
mod types;
mod xattr;

pub use address_space::{
    AddressSpace, AddressSpaceOperations, AddressSpaceViewGuard, BatchWriteRunTail,
    BatchedWriteOperations, ReadaheadControl, WriteBeginRequest, WriteEndRequest, WriteReservation,
    WritebackControl, WritebackErrors, WritebackSyncMode,
};
pub use command::EmptyCommandPolicy;
pub use direct_io::{
    DioDirection, DioMapping, DioMappingFlags, DioMappingKind, DirectIoOperations,
};
pub use fiemap::{FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags};
pub use file::{
    FMode, FileDirOperations, FileMappingOperations, FileOperations, FilePollSource, VfsFile,
    VfsFileBuilder,
};
pub(crate) use file_slot::FileSlots;
pub use file_slot::{DMA_BUF_SLOT, FileSlot};
pub use file_system_type::{
    FileSystemType, FileSystemTypeFlags, FsContextOperations, GetTreeFn, InitFsContextFn,
    ReconfigureFn, get_filesystem_type, register_filesystem, registered_filesystems,
};
pub use filename::Filename;
pub use fs_context::{FsContext, FsContextPurpose};
pub use kiocb::{Kiocb, KiocbFlags};
pub use ktime_types::TimestampLimits;
pub use locks::{FileLock, FileLockOwner, FileLockType};
pub use lookup::{LookupFlags, LookupIntent, MagicLinkOps, ResolvedObject};
pub use mount::{
    CreatedEntry, MntNamespace, Mount, MountFlags, MountIdmap, NamespaceClone, Path, RenderedPath,
};
pub use namei::{DelayedCall, LastType, ParentLookup, dentry_open, may_mknod};
pub use node::{
    CharacterDeviceRegistration, Dentry, DentryNotification, DentryOperations, DeviceAccess,
    DeviceAccessPolicy, DeviceFileOps, DirContext, DirEntrySink, FiemapCapability, FileAttr,
    FileCapabilityState, GetattrQueryFlags, GetattrRequestMask, InodeAttributeOperations,
    InodeDirOperations, InodeFiemapOperations, InodeLookupFlags, InodeOperations,
    InodeSymlinkOperations, InodeUpdateTime, LockedDentry, MmapMapper, NodeFlags, RenameFlags,
    VfsInode, VfsInodeInit, WeakVfsInode, cdev_add, cdev_add_named, cdev_del, check_device_access,
    empty_inode_operations, generic_update_time, get_next_ino, inode_init_owner,
    metadata_only_inode_operations, open_character_device, register_device_access_policy,
    registered_character_devices, subscribe_character_devices,
};
pub(crate) use node::{
    DentryKey, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive,
};
pub(crate) use open_flags::{AccMode, OpenHow, OpenParams};
pub use open_flags::{OPENAT2_REGULAR, OpenFlags, Openat2How, ResolveFlags};
pub use pagecache::WritebackRangeResult;
pub use permission::{Permission, generic_permission, open_access_to_permission};
pub use poll::poll_file_io;
pub use quota::QuotaOperations;
pub use seq_file::{
    SeekEndPolicy, SeqCommandOptions, SeqFile, SeqFileInode, SeqFileOptions, SeqIterator,
    SeqValidateFn, SeqWriteFn, seq_open,
};
pub use simple_dir::{
    ChainedDirOps, DirMaker, DirMapping, IntoDirMappingEntry, SimpleDir, SimpleDirEntry,
    SimpleDirLookup, SimpleDirOps, SparseDirSource, new_sparse_dir_inode,
};
pub use simple_file::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFileOps};
pub use simple_fs::{SimpleFs, SimpleFsNode};
pub use super_block::{
    MAX_LFS_FILESIZE, StatFs, StatFsFlags, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    SuperBlockRegistry, default_evict_inode, get_tree_bdev, get_tree_nodev, super_block_registry,
    sync_filesystems,
};
pub use transfer::{
    TransferPosition, copy_file_range, copy_file_range_as, transfer_files, transfer_files_as,
};
pub(crate) use type_map::TypeMap;
pub use types::{
    DeviceId, FallocateFlags, InodeTimes, Metadata, MetadataUpdate, NodePermission, NodeType,
    RemovedEntry, RenameOutcome, SetattrTime, Umode,
};
pub use xattr::{XATTR_NAME_MAX, XattrName, XattrNameRef, XattrNameSink, XattrSetFlags};

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use ksync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
/// Read-only page-cache reference retained until mapping teardown completes.
pub use pagecache::{MappedFolio, PinnedFolio};
