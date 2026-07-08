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
mod libfs;
mod lookup;
mod mount;
mod namei;
mod node;
mod nullfs;
mod open_flags;
pub mod path;
mod permission;
mod seq_file;
mod simple_dir;
mod simple_file;
mod simple_fs;
mod super_block;
mod type_map;
mod types;

pub use address_space::{
    AddressSpace, AddressSpaceOperations, ReadaheadControl, WriteBeginRequest, WriteEndRequest,
    WritebackControl, WritebackSyncMode,
};
pub use anon_inode::AnonInodeFs;
pub use file::{FMode, FileDirOperations, FileOperations, VfsFile, VfsFileBuilder};
pub use filename::Filename;
pub use kiocb::Kiocb;
pub use libfs::{
    simple_fsync_noflush, simple_getattr, simple_rename, simple_statfs_with_flags, simple_write_end,
};
pub use lookup::{LookupFlags, LookupIntent, MagicLinkOps, ResolvedObject};
pub use mount::{MntNamespace, Mount, MountFlags, MountIdmap, NamespaceClone, Path};
pub use namei::{DelayedCall, LastType, ParentLookup, dentry_open};
pub use node::{
    Dentry, DentryOperations, DeviceFileOps, DirContext, DirEntrySink, GetattrQueryFlags,
    GetattrRequestMask, InodeCache, InodeDirOperations, InodeLookupFlags, InodeOperations,
    InodeSymlinkOperations, MmapMapper, NodeFlags, RENAME_EXCHANGE, RENAME_NOREPLACE,
    RENAME_WHITEOUT, RenameFlags, VfsInode, VfsInodeInit, WeakVfsInode, bdev_add, bdev_del,
    cdev_add, cdev_del,
};
pub(crate) use node::{
    DentryKey, d_inode, d_is_dir, d_is_negative, d_is_symlink, d_really_is_positive,
};
pub(crate) use open_flags::{AccMode, OpenFlags, OpenHow};
pub use permission::{Permission, check_permission, open_access_to_permission};
pub use seq_file::{SeqFile, SeqFileInode, SeqIterator, seq_open};
pub use simple_dir::{
    ChainedDirOps, DirMaker, DirMapping, IntoDirMappingEntry, SimpleDir, SimpleDirEntry,
    SimpleDirLookup, SimpleDirOps,
};
pub use simple_file::{RwFile, SimpleFile, SimpleFileOperation, SimpleFileOps};
pub use simple_fs::{SimpleFs, SimpleFsNode};
pub use super_block::{
    MAX_LFS_FILESIZE, ST_NOATIME, ST_NODEV, ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_NOSYMFOLLOW,
    ST_RDONLY, ST_RELATIME, ST_VALID, StatFs, SuperBlock, SuperBlockOperations, SuperBlockRegistry,
    default_evict_inode, super_block_registry, sync_filesystems,
};
pub(crate) use type_map::TypeMap;
pub use types::{DeviceId, Metadata, MetadataUpdate, NodePermission, NodeType, Umode};

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use ksync::{Mutex, MutexGuard};
