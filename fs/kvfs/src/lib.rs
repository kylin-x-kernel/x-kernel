// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod address_space;
mod anon_inode;
mod file;
mod filename;
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

pub use address_space::{
    AddressSpace, AddressSpaceOperations, AddressSpaceViewGuard, ReadaheadControl,
    WriteBeginRequest, WriteEndRequest, WritebackControl, WritebackSyncMode,
};
pub use anon_inode::{AnonInodeFs, init_anon_inodefs};
pub use file::{FMode, FileDirOperations, FileOperations, VfsFile, VfsFileBuilder};
pub use filename::Filename;
pub use kiocb::Kiocb;
pub use lookup::{LookupFlags, LookupIntent, MagicLinkOps, ResolvedObject};
pub use mount::{MntNamespace, Mount, MountFlags, MountIdmap, NamespaceClone, Path};
pub use namei::{DelayedCall, LastType, ParentLookup, dentry_open, may_mknod};
pub use node::{
    Dentry, DentryOperations, DeviceFileOps, DirContext, DirEntrySink, GetattrQueryFlags,
    GetattrRequestMask, InodeCache, InodeDirOperations, InodeLookupFlags, InodeOperations,
    InodeSymlinkOperations, InodeUpdateTime, LockedDentry, MmapMapper, NodeFlags, RenameFlags,
    VfsInode, VfsInodeInit, WeakVfsInode, bdev_add, bdev_del, cdev_add, cdev_del, inode_init_owner,
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
    MAX_LFS_FILESIZE, StatFs, StatFsFlags, SuperBlock, SuperBlockOperations, SuperBlockRegistry,
    default_evict_inode, super_block_registry, sync_filesystems,
};
pub(crate) use type_map::TypeMap;
pub use types::{DeviceId, Metadata, MetadataUpdate, NodePermission, NodeType, SetattrTime, Umode};

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use ksync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
