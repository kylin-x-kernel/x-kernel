// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod address_space;
mod anon_inode;
mod fiemap;
mod file;
mod file_system_type;
mod filename;
mod fs_context;
mod kiocb;
pub mod libfs;
mod lookup;
mod mount;
mod namei;
mod node;
mod nullfs;
mod open_flags;
pub mod path;
mod permission;
pub mod pipe;
mod seq_file;
mod simple_dir;
mod simple_file;
mod simple_fs;
mod super_block;
mod type_map;
mod types;
mod xattr;

pub use address_space::{
    AddressSpace, AddressSpaceOperations, AddressSpaceViewGuard, PageMkwriteRequest,
    ReadaheadControl, WriteBeginRequest, WriteEndRequest, WritebackControl, WritebackSyncMode,
};
pub use anon_inode::{AnonInodeFs, init_anon_inodefs};
pub use fiemap::{FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags};
pub use file::{FMode, FileDirOperations, FileOperations, VfsFile, VfsFileBuilder};
pub use file_system_type::{
    FileSystemType, FileSystemTypeFlags, GetTreeFn, get_filesystem_type, register_filesystem,
    registered_filesystems,
};
pub use filename::Filename;
pub use fs_context::FsContext;
pub use kiocb::Kiocb;
pub use lookup::{LookupFlags, LookupIntent, MagicLinkOps, ResolvedObject};
pub use mount::{MntNamespace, Mount, MountFlags, MountIdmap, NamespaceClone, Path};
pub use namei::{DelayedCall, LastType, ParentLookup, dentry_open, may_mknod};
pub use node::{
    Dentry, DentryOperations, DeviceFileOps, DirContext, DirEntrySink, FiemapCapability,
    GetattrQueryFlags, GetattrRequestMask, InodeAttributeOperations, InodeDirOperations,
    InodeFiemapOperations, InodeLookupFlags, InodeOperations, InodeSymlinkOperations,
    InodeUpdateTime, LockedDentry, MmapMapper, NodeFlags, RenameFlags, VfsInode, VfsInodeInit,
    WeakVfsInode, cdev_add, cdev_del, inode_init_owner,
};
pub(crate) use node::{
    DentryKey, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive,
};
pub use open_flags::OpenFlags;
pub(crate) use open_flags::{AccMode, OpenHow, OpenParams};
pub use permission::{Permission, generic_permission, open_access_to_permission};
pub use seq_file::{SeqFile, SeqFileInode, SeqIterator, seq_open};
pub use simple_dir::{
    ChainedDirOps, DirMaker, DirMapping, IntoDirMappingEntry, SimpleDir, SimpleDirEntry,
    SimpleDirLookup, SimpleDirOps,
};
pub use simple_file::{RwFile, SimpleFile, SimpleFileOperation, SimpleFileOps};
pub use simple_fs::{SimpleFs, SimpleFsNode};
pub use super_block::{
    MAX_LFS_FILESIZE, StatFs, StatFsFlags, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    SuperBlockRegistry, default_evict_inode, get_tree_bdev, get_tree_nodev, super_block_registry,
    sync_filesystems,
};
pub(crate) use type_map::TypeMap;
pub use types::{DeviceId, Metadata, MetadataUpdate, NodePermission, NodeType, SetattrTime, Umode};
pub use xattr::{XATTR_NAME_MAX, XattrName, XattrNameRef, XattrNameSink, XattrSetFlags};

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use ksync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
