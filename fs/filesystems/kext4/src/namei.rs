// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style namespace mutation helpers for ext4 directories.

use alloc::{sync::Arc, vec, vec::Vec};

use crate::{
    BlockCount, BlockMapping, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, FilesystemBlock,
    InodeNumber, LogicalBlock, PhysicalBlock, UnsupportedKind,
    dirhash::DirectoryHash,
    disk::{DirectoryFileType, checksum, dir as disk_dir, inode as disk_inode},
    extent::ExtentMappingState,
    inode::{
        Ext4DeviceId, Ext4Inode, Ext4Timestamp, InodeInitialization, InodeKind, inode_checksum_seed,
    },
    jbd2::JournalCredits,
    journal::MountedJournal,
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    superblock::{metadata_access_bytes, replace_metadata_access_bytes},
    xattr::external_xattr_eviction_credits,
};

const EXT4_LINK_MAX: u16 = 65_000;

/// Credit total for the Phase C eviction finish transaction.
///
/// `update_unlinked_inode_metadata` (1) + `remove_orphan` (2) +
/// `release_allocated_inode` (8).
const EVICTION_FINISH_CREDITS: u32 = 11;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryInsertPath {
    InPlace,
    LinearAppend,
    IndexedLeafSplit,
    LinearToIndexed,
    LinearToIndexedAndSplit,
}

impl DirectoryInsertPath {
    const fn uses_htree(self) -> bool {
        matches!(
            self,
            Self::IndexedLeafSplit | Self::LinearToIndexed | Self::LinearToIndexedAndSplit
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryInsertPlan {
    required_blocks: u64,
    path: DirectoryInsertPath,
}

struct Ext4Credits;

impl Ext4Credits {
    const DIRECTORY_BLOCK_GROW: u32 = 4;
    const DIRECTORY_BLOCK_UPDATE: u32 = 1;
    const FILE_BLOCK_ALLOCATOR: u32 = 8;
    const FILE_EXTENT_UPDATE: u32 = 8;
    const HTREE_INDEX_EXTRA: u32 = 8;
    const INODE_ALLOCATOR: u32 = 8;
    const INODE_FREE: u32 = 8;
    const INODE_UPDATE: u32 = 1;
    const ORPHAN_LINK_UPDATE: u32 = 2;
    const SYMLINK_DATA_BLOCK: u32 = 1;

    fn create(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        insert: DirectoryInsertPlan,
    ) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR
                + Self::dirent_insert(directory, insert)
                + Self::DIRECTORY_BLOCK_GROW,
        )
    }

    fn mkdir(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        insert: DirectoryInsertPlan,
    ) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR
                + Self::DIRECTORY_BLOCK_UPDATE
                + Self::dirent_insert(directory, insert)
                + Self::DIRECTORY_BLOCK_GROW
                + Self::FILE_BLOCK_ALLOCATOR
                + Self::FILE_EXTENT_UPDATE
                + Self::INODE_UPDATE,
        )
    }

    fn block_mapped_symlink(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        insert: DirectoryInsertPlan,
    ) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR
                + Self::DIRECTORY_BLOCK_UPDATE
                + Self::dirent_insert(directory, insert)
                + Self::DIRECTORY_BLOCK_GROW
                + Self::INODE_UPDATE
                + Self::SYMLINK_DATA_BLOCK,
        )
    }

    fn link(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        _target: &Ext4Inode,
        insert: DirectoryInsertPlan,
    ) -> JournalCredits {
        Self::credits(Self::INODE_UPDATE + Self::dirent_insert(directory, insert))
    }

    fn unlink(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        victim: &Ext4Inode,
    ) -> JournalCredits {
        let zero_link = if victim.links_count() == 1 {
            Self::namespace_zero_link_update()
        } else {
            Self::INODE_UPDATE
        };
        Self::credits(Self::dirent_update(directory) + zero_link)
    }

    fn rmdir(
        _filesystem: &Ext4SbInfo,
        directory: &Ext4Inode,
        _victim: &Ext4Inode,
    ) -> JournalCredits {
        Self::credits(
            Self::dirent_update(directory)
                + Self::INODE_UPDATE
                + Self::namespace_zero_link_update(),
        )
    }

    fn rename(
        _filesystem: &Ext4SbInfo,
        old_directory: &Ext4Inode,
        new_directory: &Ext4Inode,
        moved: &Ext4Inode,
        replaced: Option<&Ext4Inode>,
        target_insert: Option<DirectoryInsertPlan>,
    ) -> JournalCredits {
        let parent_link_updates = if moved.kind() == InodeKind::Directory {
            Self::INODE_UPDATE * 2
        } else {
            0
        };
        let replaced_update = replaced.map_or(0, |victim| {
            if victim.links_count() == 1 || victim.kind() == InodeKind::Directory {
                Self::namespace_zero_link_update()
            } else {
                Self::INODE_UPDATE
            }
        });
        let target_dirent = target_insert.map_or_else(
            || Self::dirent_update(new_directory),
            |insert| Self::dirent_insert(new_directory, insert),
        );
        Self::credits(
            target_dirent
                + Self::dirent_update(old_directory)
                + Self::INODE_UPDATE
                + parent_link_updates
                + replaced_update,
        )
    }

    const fn credits(blocks: u32) -> JournalCredits {
        JournalCredits::new(blocks)
    }

    fn dirent_update(directory: &Ext4Inode) -> u32 {
        Self::DIRECTORY_BLOCK_UPDATE
            + Self::INODE_UPDATE
            + if directory.has_indexed_directory() {
                Self::HTREE_INDEX_EXTRA
            } else {
                0
            }
    }

    fn dirent_insert(directory: &Ext4Inode, insert: DirectoryInsertPlan) -> u32 {
        Self::DIRECTORY_BLOCK_UPDATE
            + Self::INODE_UPDATE
            + if directory.has_indexed_directory() || insert.path.uses_htree() {
                Self::HTREE_INDEX_EXTRA
            } else {
                0
            }
    }

    // Namespace removal persists the orphan before the last VFS reference
    // releases data, xattrs, and the inode in final eviction.
    const fn namespace_zero_link_update() -> u32 {
        Self::ORPHAN_LINK_UPDATE + Self::INODE_UPDATE
    }

    fn eviction_prepare(inode: &Ext4Inode) -> Ext4Result<JournalCredits> {
        Self::ORPHAN_LINK_UPDATE
            .checked_add(external_xattr_eviction_credits(inode))
            .map(Self::credits)
            .ok_or(Ext4Error::Overflow)
    }

    fn final_eviction(filesystem: &Ext4SbInfo, inode: &Ext4Inode) -> Ext4Result<JournalCredits> {
        let extent_delete = if filesystem.unlinked_inode_data_blocks(inode)? == 0 {
            0
        } else {
            filesystem.extent_truncate_metadata_credits(inode, LogicalBlock::new(0))?
        };
        let credits = Self::ORPHAN_LINK_UPDATE
            .checked_add(Self::INODE_UPDATE)
            .and_then(|credits| credits.checked_add(Self::INODE_FREE))
            .and_then(|credits| credits.checked_add(external_xattr_eviction_credits(inode)))
            .and_then(|credits| credits.checked_add(extent_delete))
            .ok_or(Ext4Error::Overflow)?;
        Ok(Self::credits(credits))
    }
}

impl Ext4SbInfo {
    /// Creates a regular file in a linear ext4 directory.
    ///
    /// This is the first R7 namei write path. It keeps ext4-specific work in
    /// the storage core: inode allocation, directory-entry update, parent
    /// timestamps, prevalidation, and transaction-wide abort on a failure
    /// after metadata publication.
    /// HTree insertion, mkdir/link/unlink/rename, and zero-link eviction remain
    /// separate R7 steps.
    pub fn create_regular_file(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }
        self.validate_inode_timestamp_update(parent, timestamp)?;
        let insert = self.ensure_namespace_insert_capacity(parent, name, 0)?;

        let credits = Ext4Credits::create(self, parent, insert);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.create_regular_file_in_transaction(
            parent,
            name,
            permissions,
            uid,
            gid,
            timestamp,
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }

    /// Creates a subdirectory in a linear ext4 directory.
    pub fn create_directory(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }
        self.validate_inode_timestamp_update(parent, timestamp)?;
        parent
            .links_count()
            .checked_add(1)
            .filter(|links| *links <= EXT4_LINK_MAX)
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::LinkCountLimit))?;
        let insert = self.ensure_namespace_insert_capacity(parent, name, 1)?;

        let credits = Ext4Credits::mkdir(self, parent, insert);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.create_directory_in_transaction(
            parent,
            name,
            permissions,
            uid,
            gid,
            timestamp,
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }

    /// Creates a symbolic link in a linear ext4 directory.
    pub fn create_symlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        validate_symlink_target(target)?;
        if target.len() < disk_inode::INODE_BLOCK_BYTES {
            return self.create_fast_symlink(parent, name, target, uid, gid, timestamp);
        }
        self.create_block_mapped_symlink(parent, name, target, uid, gid, timestamp)
    }

    /// Creates a fast symbolic link in a linear ext4 directory.
    pub fn create_fast_symlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        let initialization =
            InodeInitialization::fast_symlink(target, uid, gid)?.with_timestamp(timestamp);
        self.create_initialized_child(
            parent,
            name,
            initialization,
            DirectoryFileType::Symlink,
            timestamp,
        )
    }

    /// Creates a single-block extent-backed symbolic link in a linear ext4 directory.
    pub fn create_block_mapped_symlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        validate_symlink_target(target)?;
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        if target.len() > block_size {
            return Err(Ext4Error::Unsupported(UnsupportedKind::BlockMappedSymlink));
        }
        if !self.superblock().features().has_extents() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }
        self.validate_inode_timestamp_update(parent, timestamp)?;
        let insert = self.ensure_namespace_insert_capacity(parent, name, 1)?;

        let credits = Ext4Credits::block_mapped_symlink(self, parent, insert);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.create_block_mapped_symlink_in_transaction(
            parent,
            name,
            target,
            uid,
            gid,
            timestamp,
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }

    /// Creates a FIFO, socket, character device, or block device in a linear directory.
    #[allow(clippy::too_many_arguments)]
    pub fn create_special_file(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        special: (InodeKind, Option<Ext4DeviceId>),
        permissions: u16,
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        let (kind, device) = special;
        let initialization = InodeInitialization::special(kind, permissions, device, uid, gid)?
            .with_timestamp(timestamp);
        let file_type = directory_file_type_for_inode_kind(kind);
        self.create_initialized_child(parent, name, initialization, file_type, timestamp)
    }

    /// Adds a hard link to an existing non-directory inode in a linear directory.
    pub fn link(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.ensure_namespace_mutation_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }
        if target.kind() == InodeKind::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if target.links_count() >= EXT4_LINK_MAX {
            return Err(Ext4Error::Unsupported(UnsupportedKind::LinkCountLimit));
        }
        self.validate_inode_timestamp_update(parent, timestamp)?;
        self.validate_inode_timestamp_update(target, timestamp)?;
        let file_type = directory_file_type_for_inode_kind(target.kind());
        let insert = self.ensure_namespace_insert_capacity(parent, name, 0)?;

        let credits = Ext4Credits::link(self, parent, target, insert);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result =
            self.link_in_transaction(parent, name, target, file_type, timestamp, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    /// Unlinks a non-directory child from a linear ext4 directory.
    ///
    /// # Errors
    ///
    /// Returns a typed namespace, ownership, format, journal, or I/O error.
    /// The supplied child must match the directory entry selected by `name`.
    pub fn unlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.ensure_namespace_mutation_supported(parent, name)?;
        let entry = self
            .lookup_bytes(parent, name)?
            .ok_or(Ext4Error::NotFound)?;
        if entry.file_type() == DirectoryFileType::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if entry.inode() != child.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if child.kind() == InodeKind::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        self.ensure_unlinked_inode_eviction_supported(child)?;
        self.validate_inode_timestamp_update(parent, timestamp)?;
        self.validate_inode_timestamp_update(child, timestamp)?;

        let credits = Ext4Credits::unlink(self, parent, child);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.unlink_in_transaction(parent, name, child, timestamp, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    /// Removes an empty subdirectory from a linear ext4 directory.
    ///
    /// # Errors
    ///
    /// Returns a typed namespace, ownership, format, journal, or I/O error.
    /// The supplied child must match the directory entry selected by `name`.
    pub fn remove_directory(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.ensure_namespace_mutation_supported(parent, name)?;
        let entry = self
            .lookup_bytes(parent, name)?
            .ok_or(Ext4Error::NotFound)?;
        if entry.file_type() != DirectoryFileType::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if entry.inode() != child.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if child.kind() != InodeKind::Directory {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.ensure_empty_linear_directory(parent, child)?;
        self.ensure_unlinked_inode_eviction_supported(child)?;
        self.validate_inode_timestamp_update(parent, timestamp)?;
        self.validate_inode_timestamp_update(child, timestamp)?;

        let credits = Ext4Credits::rmdir(self, parent, child);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result =
            self.remove_directory_in_transaction(parent, name, child, timestamp, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    /// Renames a child between supported linear ext4 directories.
    ///
    /// # Errors
    ///
    /// Returns a typed namespace, ownership, format, journal, or I/O error.
    /// The supplied inodes must be the VFS-resident participants matching the
    /// source and target directory entries.
    #[expect(
        clippy::too_many_arguments,
        reason = "rename receives the complete set of VFS-locked namespace participants"
    )]
    pub fn rename(
        &mut self,
        source_parent: &Ext4Inode,
        source_name: &[u8],
        moved: &Ext4Inode,
        target_parent: &Ext4Inode,
        target_name: &[u8],
        replaced: Option<&Ext4Inode>,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        self.ensure_namespace_mutation_supported(source_parent, source_name)?;
        self.ensure_namespace_mutation_supported(target_parent, target_name)?;
        let same_parent = source_parent.number() == target_parent.number();
        if same_parent && source_name == target_name {
            let entry = self
                .lookup_bytes(source_parent, source_name)?
                .ok_or(Ext4Error::NotFound)?;
            if entry.inode() != moved.number() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            return Ok(());
        }

        let source_entry = self
            .lookup_bytes(source_parent, source_name)?
            .ok_or(Ext4Error::NotFound)?;
        if source_entry.inode() != moved.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let target_entry = self.lookup_bytes(target_parent, target_name)?;
        match (target_entry.as_ref(), replaced) {
            (None, None) => {}
            (Some(entry), Some(replaced)) if entry.inode() == replaced.number() => {}
            _ => return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry)),
        }

        if replaced.is_some_and(|target| target.number() == moved.number()) {
            return Ok(());
        }

        self.validate_inode_timestamp_update(source_parent, timestamp)?;
        if !same_parent {
            self.validate_inode_timestamp_update(target_parent, timestamp)?;
        }
        self.validate_inode_timestamp_update(moved, timestamp)?;
        if let Some(target) = replaced {
            self.validate_inode_timestamp_update(target, timestamp)?;
        }
        self.ensure_rename_type_supported(moved, replaced)?;
        if let Some(target) = replaced {
            if target.kind() == InodeKind::Directory {
                self.ensure_empty_linear_directory(target_parent, target)?;
                self.ensure_unlinked_inode_eviction_supported(target)?;
            } else if target.links_count() == 1 {
                self.ensure_unlinked_inode_eviction_supported(target)?;
            }
        }
        let target_insert = if replaced.is_none() {
            Some(self.ensure_namespace_insert_capacity(target_parent, target_name, 0)?)
        } else {
            None
        };
        if moved.kind() == InodeKind::Directory
            && !same_parent
            && replaced.is_none_or(|target| target.kind() != InodeKind::Directory)
        {
            target_parent
                .links_count()
                .checked_add(1)
                .filter(|links| *links <= EXT4_LINK_MAX)
                .ok_or(Ext4Error::Unsupported(UnsupportedKind::LinkCountLimit))?;
        }
        let credits = Ext4Credits::rename(
            self,
            source_parent,
            target_parent,
            moved,
            replaced,
            target_insert,
        );
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.rename_in_transaction(
            source_parent,
            source_name,
            target_parent,
            target_name,
            moved,
            replaced,
            timestamp,
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_regular_file_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::regular_file(permissions, uid, gid).with_timestamp(timestamp),
            handle,
        )?;
        let child = self.internal_iget(allocation.inode())?;
        self.insert_directory_entry(
            parent,
            name,
            child.number(),
            DirectoryFileType::RegularFile,
            timestamp,
            handle,
        )?;
        Ok(child)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_directory_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::directory(permissions, uid, gid).with_timestamp(timestamp),
            handle,
        )?;
        let child = self.internal_iget(allocation.inode())?;
        self.initialize_directory_data_block(&child, parent.number(), timestamp, handle)?;
        self.insert_directory_entry(
            parent,
            name,
            child.number(),
            DirectoryFileType::Directory,
            timestamp,
            handle,
        )?;
        let parent_links = parent
            .links_count()
            .checked_add(1)
            .ok_or(Ext4Error::Overflow)?;
        self.update_inode_links_count_metadata(parent, parent_links, timestamp, handle)?;
        Ok(child)
    }

    fn create_initialized_child(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        initialization: InodeInitialization,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }
        self.validate_inode_timestamp_update(parent, timestamp)?;
        let insert = self.ensure_namespace_insert_capacity(parent, name, 0)?;

        let credits = Ext4Credits::create(self, parent, insert);
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = self.create_initialized_child_in_transaction(
            parent,
            name,
            initialization,
            file_type,
            timestamp,
            &mut handle,
        );
        self.complete_metadata_mutation(handle, result)
    }

    fn create_initialized_child_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        initialization: InodeInitialization,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let allocation =
            self.allocate_named_inode(Some(parent.number()), name, initialization, handle)?;
        let child = self.internal_iget(allocation.inode())?;
        self.insert_directory_entry(parent, name, child.number(), file_type, timestamp, handle)?;
        Ok(child)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_block_mapped_symlink_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::block_mapped_symlink(target.len(), uid, gid)?
                .with_timestamp(timestamp),
            handle,
        )?;
        let child = self.internal_iget(allocation.inode())?;
        self.initialize_symlink_data_block(&child, target, handle)?;
        self.insert_directory_entry(
            parent,
            name,
            child.number(),
            DirectoryFileType::Symlink,
            timestamp,
            handle,
        )?;
        Ok(child)
    }

    fn link_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &Ext4Inode,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let links_count = target
            .links_count()
            .checked_add(1)
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::LinkCountLimit))?;
        self.update_inode_links_count_ctime_metadata(target, links_count, timestamp, handle)?;
        self.insert_directory_entry(parent, name, target.number(), file_type, timestamp, handle)
    }

    fn unlink_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let removed = self.remove_directory_entry(parent, name, timestamp, handle)?;
        if removed.inode != child.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        self.finish_removed_inode(child, None, timestamp, handle)
    }

    fn remove_directory_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let removed = self.remove_directory_entry(parent, name, timestamp, handle)?;
        if removed.inode != child.number() || removed.file_type != DirectoryFileType::Directory {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let parent_links = parent
            .links_count()
            .checked_sub(1)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
        self.update_inode_links_count_metadata(parent, parent_links, timestamp, handle)?;
        self.finish_removed_inode(child, Some(0), timestamp, handle)
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_in_transaction(
        &mut self,
        source_parent: &Ext4Inode,
        source_name: &[u8],
        target_parent: &Ext4Inode,
        target_name: &[u8],
        moved: &Ext4Inode,
        replaced: Option<&Ext4Inode>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let same_parent = source_parent.number() == target_parent.number();
        let file_type = directory_file_type_for_inode_kind(moved.kind());
        let replaced_entry = if let Some(replaced) = replaced {
            let entry = self.replace_directory_entry(
                target_parent,
                target_name,
                moved.number(),
                file_type,
                timestamp,
                handle,
            )?;
            if entry.inode != replaced.number()
                || entry.file_type != directory_file_type_for_inode_kind(replaced.kind())
            {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            Some(entry)
        } else {
            self.insert_directory_entry(
                target_parent,
                target_name,
                moved.number(),
                file_type,
                timestamp,
                handle,
            )?;
            None
        };
        self.update_inode_ctime_metadata(moved, timestamp, handle)?;

        let removed = self.remove_directory_entry(source_parent, source_name, timestamp, handle)?;
        if removed.inode != moved.number() || removed.file_type != file_type {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        if moved.kind() != InodeKind::Directory {
            if let Some(replaced) = replaced {
                self.finish_replaced_inode(replaced, replaced_entry, timestamp, handle)?;
            }
            return Ok(());
        }

        let replaced_directory = replaced.is_some_and(|inode| inode.kind() == InodeKind::Directory);
        if !same_parent || replaced_directory {
            let source_links = source_parent
                .links_count()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
            self.update_inode_links_count_metadata(source_parent, source_links, timestamp, handle)?;
        }

        if !same_parent && !replaced_directory {
            let target_links = target_parent
                .links_count()
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?;
            self.update_inode_links_count_metadata(target_parent, target_links, timestamp, handle)?;
        }

        if !same_parent {
            self.update_directory_dotdot_entry(moved, target_parent.number(), timestamp, handle)?;
        }

        if let Some(replaced) = replaced {
            self.finish_replaced_inode(replaced, replaced_entry, timestamp, handle)?;
        }
        Ok(())
    }

    fn finish_replaced_inode(
        &mut self,
        inode: &Ext4Inode,
        replaced_entry: Option<ReplacedDirectoryEntry>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let replaced_entry =
            replaced_entry.ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        if replaced_entry.inode != inode.number()
            || replaced_entry.file_type != directory_file_type_for_inode_kind(inode.kind())
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let size = if inode.kind() == InodeKind::Directory {
            Some(0)
        } else {
            None
        };
        self.finish_removed_inode(inode, size, timestamp, handle)
    }

    fn finish_removed_inode(
        &mut self,
        inode: &Ext4Inode,
        zero_link_size: Option<u64>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if inode.kind() != InodeKind::Directory && inode.links_count() > 1 {
            let links_count = inode
                .links_count()
                .checked_sub(1)
                .ok_or(Ext4Error::Overflow)?;
            return self.update_inode_links_count_ctime_metadata(
                inode,
                links_count,
                timestamp,
                handle,
            );
        }
        self.add_namespace_orphan(inode, handle)?;
        self.update_unlinked_inode_metadata(inode, zero_link_size, timestamp, handle)
    }

    /// Releases a zero-link inode after its final VFS reference is gone.
    ///
    /// # Errors
    ///
    /// Format, journal, and device errors are propagated.
    #[cfg(test)]
    pub fn evict_unlinked_inode(
        &mut self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        if inode.links_count() != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.evict_unlinked_inode_with_policy(
            inode,
            timestamp,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn cleanup_unlinked_orphan_from_head(
        &mut self,
        inode: &Ext4Inode,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        if self.orphan_head() != Some(inode.number()) || inode.links_count() != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.evict_unlinked_inode_with_policy(inode, inode.ctime(), recovery_flag_policy)
    }

    fn evict_unlinked_inode_with_policy(
        &mut self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.ensure_unlinked_inode_eviction_supported(inode)?;
        self.validate_inode_timestamp_update(inode, timestamp)?;

        let credits = Ext4Credits::final_eviction(self, inode)?;
        let journal = self.namei_metadata_journal_with_policy(credits, recovery_flag_policy)?;
        let mut handle = journal.begin(credits)?;
        let result = self.evict_zero_link_inode(inode, None, timestamp, &mut handle);
        self.complete_metadata_mutation_with_policy(handle, result, recovery_flag_policy)
    }

    // ------------------------------------------------------------------
    //  Three-phase eviction API
    // ------------------------------------------------------------------

    /// Phase A — prepare eviction.
    ///
    /// In a single journal transaction:
    /// 1. Read and verify the zero-link inode.
    /// 2. `add_namespace_orphan` — persist the orphan link.
    /// 3. `release_external_xattr_block_for_eviction` — free the xattr block.
    ///
    /// The extent tree is **not** truncated in this phase.  It remains on disk
    /// as the persistent record of blocks to free.  If a crash occurs before
    /// Phase B completes, the orphan inode recovery will find the extent tree
    /// and finish the truncation.
    ///
    /// After this call the inode is orphaned.  The caller should release the
    /// `KExt4Core` lock before calling Phase B.
    pub fn eviction_prepare(&mut self, inode: &Ext4Inode) -> Ext4Result<()> {
        if inode.links_count() != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.ensure_unlinked_inode_eviction_supported(inode)?;

        let credits = Ext4Credits::eviction_prepare(inode)?;
        let journal = self.namei_metadata_journal(credits)?;
        let mut handle = journal.begin(credits)?;
        let result = (|| -> Ext4Result<()> {
            self.add_namespace_orphan(inode, &mut handle)?;
            self.release_external_xattr_block_for_eviction(inode, &mut handle)
        })();

        self.complete_metadata_mutation_and_commit_with_policy(
            handle,
            result,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    /// Phase B — atomically truncate extent tree in batches.
    ///
    /// Each batch removes at most `max_blocks` physical blocks' worth of
    /// extent mappings from the tail of the file and releases the underlying
    /// physical blocks in a single atomic journal transaction.  This approach
    /// is crash-safe: the extent tree on disk always reflects exactly the
    /// blocks that have not yet been freed.
    ///
    /// Returns `(freed, done)` where:
    /// - `freed` is the number of blocks actually released (best-effort
    ///   upper bound; the actual count may be slightly higher if an extent
    ///   crosses the `max_blocks` boundary),
    /// - `done` is `true` when the extent tree is fully empty.
    ///
    /// The caller should release the `KExt4Core` lock between batches.
    pub fn eviction_release_batch(
        &mut self,
        inode: &Ext4Inode,
        max_blocks: u32,
    ) -> Ext4Result<(u32, bool)> {
        // Fast check: no data blocks remaining.
        if self.unlinked_inode_data_blocks(inode)? == 0 {
            return Ok((0, true));
        }

        // Collect current extent tree to determine how much to release.
        let collected = self.collect_extent_tree(inode)?;

        // From the tail, accumulate at most `max_blocks` physical blocks.
        let mut physical_count = 0u32;
        let mut keep_count = collected.extents.len();
        for i in (0..collected.extents.len()).rev() {
            let extent_blocks = collected.extents[i].len;
            if physical_count.saturating_add(extent_blocks) > max_blocks {
                break;
            }
            physical_count += extent_blocks;
            keep_count = i;
        }

        // Determine the truncation point.
        let (new_blocks, estimated_freed) = if keep_count == collected.extents.len() {
            // Even the last extent alone exceeds max_blocks.  Truncate a
            // portion of it by computing a logical block inside the extent.
            let last = collected
                .extents
                .last()
                .copied()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidExtent))?;
            let release_len = max_blocks.min(last.len);
            let keep_logical = last
                .logical
                .checked_add(last.len.saturating_sub(release_len))
                .ok_or(Ext4Error::Overflow)?;
            (LogicalBlock::new(u64::from(keep_logical)), release_len)
        } else if keep_count == 0 {
            // All remaining extents fit in this batch.
            (LogicalBlock::new(0), physical_count)
        } else {
            // Truncate from the first extent in the release batch.
            let start = collected.extents[keep_count].logical;
            (LogicalBlock::new(u64::from(start)), physical_count)
        };

        // Get conservative credits for this truncation scope, reusing the tree
        // already collected above to avoid a second full tree walk.
        let credits = self.extent_truncate_metadata_credits_from(inode, &collected, new_blocks)?;
        // Fast check above confirmed there are data blocks, so a zero-credit
        // result means the extent tree is inconsistent with the inode block
        // count — refuse to proceed.
        if credits == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
        }

        let credits = JournalCredits::new(credits);
        let journal = self.namei_metadata_journal(credits)?;
        let mut jh = journal.begin(credits)?;

        let result = (|| -> Ext4Result<(u32, bool)> {
            self.truncate_extent_mappings_with(inode, &collected, new_blocks, &mut jh)?;
            let done = self.unlinked_inode_data_blocks(inode)? == 0;
            let freed = estimated_freed;
            Ok((freed, done))
        })();

        self.complete_metadata_mutation_and_commit_with_policy(
            jh,
            result,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    /// Phase C — finish eviction.
    ///
    /// In a single journal transaction:
    /// 1. `update_unlinked_inode_metadata` — update ctime (i_size left as-is;
    ///    orphan recovery will handle it on crash).
    /// 2. `remove_orphan` — remove from the orphan list.
    /// 3. `release_allocated_inode` — free the inode table slot + bit.
    pub fn eviction_finish(
        &mut self,
        inode: &Ext4Inode,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        // Phase B must have emptied the extent tree.
        if self.unlinked_inode_data_blocks(inode)? != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }

        let credits = JournalCredits::new(EVICTION_FINISH_CREDITS);
        let journal = self.namei_metadata_journal(credits)?;
        let mut jh = journal.begin(credits)?;

        let result = (|| -> Ext4Result<()> {
            self.update_unlinked_inode_metadata(inode, None, timestamp, &mut jh)?;
            self.remove_orphan(inode, &mut jh)?;
            self.release_allocated_inode(inode.number(), inode.kind(), &mut jh)?;
            Ok(())
        })();

        self.complete_metadata_mutation_and_commit_with_policy(
            jh,
            result,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn initialize_directory_data_block(
        &mut self,
        directory: &Ext4Inode,
        parent: InodeNumber,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let allocation = self.allocate_block(None, handle)?;
        let block = FilesystemBlock::new(allocation.block().get());
        let access = self.metadata_io.create_access(block, handle)?;
        let mut bytes = initial_directory_block_bytes(
            block_size,
            directory.number(),
            parent,
            self.superblock().features().has_metadata_checksum(),
        )?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;

        self.insert_extent_mapping(
            directory,
            LogicalBlock::new(0),
            allocation.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        self.update_inode_size_metadata(directory, block_size_u64, timestamp, handle)
    }

    fn initialize_symlink_data_block(
        &mut self,
        symlink: &Ext4Inode,
        target: &[u8],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let locality_group = self.block_group_for_inode(symlink.number())?;
        let allocation = self.allocate_blocks_for_write(
            Ext4AllocationRequest::new(
                LogicalBlock::new(0),
                None,
                BlockCount::new(1),
                BlockCount::new(1),
                Ext4AllocationFlags::EXACT,
                locality_group,
            )?,
            handle,
        )?;
        let block = FilesystemBlock::new(allocation.physical_start().get());
        let mut bytes = vec![0; block_size];
        bytes
            .get_mut(..target.len())
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
            .copy_from_slice(target);
        // Write the symlink target as a journaled metadata buffer, aligned
        // with Linux ext4_init_symlink_block() which uses ext4_bread +
        // journaled buffer_head. The buffer is checkpointed to the home
        // block during transaction commit, making the content recoverable
        // on journal replay.
        let access = self.metadata_io.create_access(block, handle)?;
        replace_metadata_access_bytes(&access, bytes)?;

        self.insert_extent_mapping(
            symlink,
            LogicalBlock::new(0),
            allocation.physical_start(),
            allocation.block_count(),
            ExtentMappingState::Initialized,
            handle,
        )
    }

    fn ensure_namespace_create_supported(&self, parent: &Ext4Inode, name: &[u8]) -> Ext4Result<()> {
        self.ensure_namespace_mutation_supported(parent, name)
    }

    fn ensure_namespace_mutation_supported(
        &self,
        parent: &Ext4Inode,
        name: &[u8],
    ) -> Ext4Result<()> {
        if parent.kind() != InodeKind::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        validate_new_entry_name(name)
    }

    fn ensure_empty_linear_directory(
        &self,
        parent: &Ext4Inode,
        directory: &Ext4Inode,
    ) -> Ext4Result<()> {
        let mut has_dot = false;
        let mut has_dotdot = false;
        for entry in self.read_dir(directory)? {
            match entry.name_bytes() {
                b"." if entry.inode() == directory.number()
                    && entry.file_type() == DirectoryFileType::Directory =>
                {
                    has_dot = true;
                }
                b".."
                    if entry.inode() == parent.number()
                        && entry.file_type() == DirectoryFileType::Directory =>
                {
                    has_dotdot = true;
                }
                b"." | b".." => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
                }
                _ => return Err(Ext4Error::DirectoryNotEmpty),
            }
        }
        if !has_dot || !has_dotdot {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(())
    }

    fn ensure_rename_type_supported(
        &self,
        moved: &Ext4Inode,
        replaced: Option<&Ext4Inode>,
    ) -> Ext4Result<()> {
        if let Some(replaced) = replaced {
            match (
                moved.kind() == InodeKind::Directory,
                replaced.kind() == InodeKind::Directory,
            ) {
                (true, false) | (false, true) => {
                    return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
                }
                _ => {}
            }
        }
        if moved.kind() == InodeKind::Directory {
            return Ok(());
        }
        match moved.kind() {
            InodeKind::RegularFile
            | InodeKind::Symlink
            | InodeKind::CharacterDevice
            | InodeKind::BlockDevice
            | InodeKind::Fifo
            | InodeKind::Socket => Ok(()),
            InodeKind::Directory => unreachable!(),
        }
    }

    fn ensure_unlinked_inode_eviction_supported(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        match inode.kind() {
            InodeKind::RegularFile
            | InodeKind::Directory
            | InodeKind::Symlink
            | InodeKind::CharacterDevice
            | InodeKind::BlockDevice
            | InodeKind::Fifo
            | InodeKind::Socket => {}
        }
        if self.unlinked_inode_data_blocks(inode)? != 0 && !inode.has_extents() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        Ok(())
    }

    fn unlinked_inode_data_blocks(&self, inode: &Ext4Inode) -> Ext4Result<u64> {
        let mut blocks = inode.blocks();
        if inode.file_acl_block() != 0 {
            let xattr_blocks = u64::from(self.layout().block_size()) / 512;
            blocks = blocks
                .checked_sub(xattr_blocks)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
        }
        Ok(blocks)
    }

    fn evict_zero_link_inode(
        &mut self,
        inode: &Ext4Inode,
        size: Option<u64>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        self.ensure_unlinked_inode_eviction_supported(inode)?;
        self.add_namespace_orphan(inode, handle)?;
        self.release_external_xattr_block_for_eviction(inode, handle)?;
        if self.unlinked_inode_data_blocks(inode)? != 0 {
            self.truncate_extent_mappings(inode, LogicalBlock::new(0), handle)?;
        }
        self.update_unlinked_inode_metadata(inode, size, timestamp, handle)?;
        self.remove_orphan(inode, handle)?;
        self.release_allocated_inode(inode.number(), inode.kind(), handle)?;
        Ok(())
    }

    fn update_directory_dotdot_entry(
        &mut self,
        directory: &Ext4Inode,
        parent: InodeNumber,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut block = vec![0; block_size];
        let physical = match self.map_blocks(directory, LogicalBlock::new(0))? {
            BlockMapping::Mapped { physical, len, .. } if physical.get() != 0 && len.get() != 0 => {
                physical
            }
            BlockMapping::Mapped { .. } => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
        };
        let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
        block.copy_from_slice(&buffer.as_ref()[..block_size]);
        if directory.has_indexed_directory() {
            let (_, count_limit) = self.decode_htree_root(&block, block_size)?;
            self.verify_htree_block_checksum(
                directory,
                0,
                &block,
                disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
                count_limit,
            )?;
        } else {
            self.verify_directory_block(directory, 0, &block)?;
        }
        if find_linear_entry_slot(&block, block_size, b"..", self.superblock().inodes_count())?
            .is_none()
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let access = self
            .metadata_io
            .write_access(FilesystemBlock::new(physical.get()), handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        let root_count_limit = if directory.has_indexed_directory() {
            let (_, count_limit) = self.decode_htree_root(&bytes, block_size)?;
            self.verify_htree_block_checksum(
                directory,
                0,
                &bytes,
                disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
                count_limit,
            )?;
            Some(count_limit)
        } else {
            self.verify_directory_block(directory, 0, &bytes)?;
            None
        };
        let slot =
            find_linear_entry_slot(&bytes, block_size, b"..", self.superblock().inodes_count())?
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        if slot.file_type != DirectoryFileType::Directory {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        put_u32(&mut bytes, slot.offset, parent.get())?;
        if let Some(count_limit) = root_count_limit {
            self.update_htree_block_checksum(
                directory,
                &mut bytes,
                disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
                count_limit,
            )?;
        } else {
            self.update_directory_block_checksum(directory, &mut bytes)?;
        }
        replace_metadata_access_bytes(&access, bytes)?;
        self.update_inode_timestamps_metadata(directory, timestamp, handle)
    }

    fn insert_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if directory.has_indexed_directory() {
            return self.insert_indexed_directory_entry(
                directory, name, inode, file_type, timestamp, handle,
            );
        }
        self.insert_linear_directory_entry(directory, name, inode, file_type, timestamp, handle)
    }

    fn ensure_namespace_insert_capacity(
        &self,
        directory: &Ext4Inode,
        name: &[u8],
        additional_blocks: u64,
    ) -> Ext4Result<DirectoryInsertPlan> {
        let plan = self.preflight_directory_entry_insert(directory, name)?;
        let required = plan
            .required_blocks
            .checked_add(additional_blocks)
            .ok_or(Ext4Error::Overflow)?;
        if required > self.free_blocks_count() {
            return Err(Ext4Error::NoSpace);
        }
        Ok(plan)
    }

    fn preflight_directory_entry_insert(
        &self,
        directory: &Ext4Inode,
        name: &[u8],
    ) -> Ext4Result<DirectoryInsertPlan> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;

        if directory.has_indexed_directory() {
            let target = self.probe_htree_insert_target(directory, name, block_size)?;
            let mut leaf = vec![0; block_size];
            let read_len = self.read_directory_block_for_write(
                directory,
                target.leaf_logical,
                target.leaf_physical,
                block_size,
                &mut leaf,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if find_linear_insert_slot(&leaf, block_size, name.len())?.is_some() {
                return Ok(DirectoryInsertPlan {
                    required_blocks: 0,
                    path: DirectoryInsertPath::InPlace,
                });
            }
            if usize::from(target.parent.count_limit.count())
                >= usize::from(target.parent.count_limit.limit())
            {
                return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
            }

            let mut records =
                self.collect_leaf_records_for_split(&leaf, block_size, target.hash_version)?;
            records.sort_by_key(|record| record.hash);
            let split = htree_leaf_split_index(&records, block_size)?;
            let hash2 = records
                .get(split)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?
                .hash;
            let selected_records = if target.hash.major() >= hash2 {
                &records[split..]
            } else {
                &records[..split]
            };
            let mut selected_leaf = vec![0; block_size];
            write_leaf_records(
                &mut selected_leaf,
                selected_records,
                block_size,
                self.superblock().features().has_metadata_checksum(),
            )?;
            if find_linear_insert_slot(&selected_leaf, block_size, name.len())?.is_none() {
                return Err(Ext4Error::NoSpace);
            }

            let logical = directory_block_count_exact(directory.size(), block_size_u64)?;
            let extent_blocks = self.extent_insert_metadata_block_bound(
                directory,
                LogicalBlock::new(logical),
                BlockCount::new(1),
            )?;
            return Ok(DirectoryInsertPlan {
                required_blocks: extent_blocks.checked_add(1).ok_or(Ext4Error::Overflow)?,
                path: DirectoryInsertPath::IndexedLeafSplit,
            });
        }

        let block_count = directory_block_count_exact(directory.size(), block_size_u64)?;
        if block_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let mut block = vec![0; block_size];
        for logical in 0..block_count {
            let physical = self.mapped_directory_block(directory, logical)?;
            let read_len = self.read_directory_block_for_write(
                directory, logical, physical, block_size, &mut block,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if find_linear_insert_slot(&block, block_size, name.len())?.is_some() {
                return Ok(DirectoryInsertPlan {
                    required_blocks: 0,
                    path: DirectoryInsertPath::InPlace,
                });
            }
        }

        let mut additional_directory_blocks = BlockCount::new(1);
        let mut insert_path = DirectoryInsertPath::LinearAppend;
        if block_count == 1 && self.superblock().features().has_dir_index() {
            insert_path = DirectoryInsertPath::LinearToIndexed;
            let conversion = collect_linear_records_for_index_conversion(
                &block,
                block_size,
                self.superblock().inodes_count(),
            )?;
            let mut leaf = vec![0; block_size];
            write_leaf_records(
                &mut leaf,
                &conversion.records,
                block_size,
                self.superblock().features().has_metadata_checksum(),
            )?;
            // Conversion first moves the linear entries into one leaf. If the
            // pending entry does not fit there, indexed insertion immediately
            // splits that leaf and therefore needs a second directory block.
            if find_linear_insert_slot(&leaf, block_size, name.len())?.is_none() {
                additional_directory_blocks = BlockCount::new(2);
                insert_path = DirectoryInsertPath::LinearToIndexedAndSplit;
            }
        }

        let extent_blocks = if insert_path == DirectoryInsertPath::LinearToIndexedAndSplit {
            self.extent_insert_independent_blocks_metadata_bound(
                directory,
                LogicalBlock::new(block_count),
                additional_directory_blocks,
            )?
        } else {
            self.extent_insert_metadata_block_bound(
                directory,
                LogicalBlock::new(block_count),
                additional_directory_blocks,
            )?
        };
        Ok(DirectoryInsertPlan {
            required_blocks: extent_blocks
                .checked_add(u64::from(additional_directory_blocks.get()))
                .ok_or(Ext4Error::Overflow)?,
            path: insert_path,
        })
    }

    fn replace_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ReplacedDirectoryEntry> {
        if directory.has_indexed_directory() {
            return self.replace_indexed_directory_entry(
                directory, name, inode, file_type, timestamp, handle,
            );
        }
        self.replace_linear_directory_entry(directory, name, inode, file_type, timestamp, handle)
    }

    fn replace_linear_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ReplacedDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_count = directory_block_count_exact(directory.size(), block_size_u64)?;

        let mut block = vec![0; block_size];
        for logical in 0..block_count {
            let physical = self.mapped_directory_block(directory, logical)?;
            let read_len = self.read_directory_block_for_write(
                directory, logical, physical, block_size, &mut block,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if find_linear_entry_slot(&block, block_size, name, self.superblock().inodes_count())?
                .is_none()
            {
                continue;
            }

            return self.replace_directory_entry_in_block(
                directory, logical, physical, name, inode, file_type, timestamp, handle,
            );
        }

        Err(Ext4Error::NotFound)
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_directory_entry_in_block(
        &mut self,
        directory: &Ext4Inode,
        logical: u64,
        physical: PhysicalBlock,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ReplacedDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let filesystem_block = FilesystemBlock::new(physical.get());
        let access = self.metadata_io.write_access(filesystem_block, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        self.verify_directory_block(directory, logical, &bytes)?;
        let slot =
            find_linear_entry_slot(&bytes, block_size, name, self.superblock().inodes_count())?
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        let replaced = slot.replace(&mut bytes, inode, file_type)?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
        Ok(ReplacedDirectoryEntry {
            inode: replaced.inode,
            file_type: replaced.file_type,
        })
    }

    fn insert_linear_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        if directory.size() == 0 || !directory.size().is_multiple_of(block_size_u64) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let block_count = directory
            .size()
            .checked_div(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;

        let mut block = vec![0; block_size];
        for logical in 0..block_count {
            let physical = match self.map_blocks(directory, LogicalBlock::new(logical))? {
                BlockMapping::Mapped { physical, len, .. }
                    if physical.get() != 0 && len.get() != 0 =>
                {
                    physical
                }
                BlockMapping::Mapped { .. } => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                }
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
                }
            };
            let read_len = self.read_directory_block_for_write(
                directory, logical, physical, block_size, &mut block,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if let Some(slot) = find_linear_insert_slot(&block, block_size, name.len())? {
                let filesystem_block = FilesystemBlock::new(physical.get());
                let access = self.metadata_io.write_access(filesystem_block, handle)?;
                let mut bytes = metadata_access_bytes(&access)?;
                self.verify_directory_block(directory, logical, &bytes)?;
                slot.write(&mut bytes, block_size, inode, file_type, name)?;
                self.update_directory_block_checksum(directory, &mut bytes)?;
                replace_metadata_access_bytes(&access, bytes)?;
                return self.update_inode_timestamps_metadata(directory, timestamp, handle);
            }
        }

        if block_count == 1 && self.superblock().features().has_dir_index() {
            return self.make_indexed_directory_and_insert(
                directory, name, inode, file_type, timestamp, handle,
            );
        }

        self.append_linear_directory_entry(directory, name, inode, file_type, timestamp, handle)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_indexed_directory_and_insert(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let root_physical = self.mapped_directory_block(directory, 0)?;
        let mut old_root = vec![0; block_size];
        let root_buffer = self.read_metadata_block(FilesystemBlock::new(root_physical.get()))?;
        old_root.copy_from_slice(&root_buffer.as_ref()[..block_size]);
        self.verify_directory_block(directory, 0, &old_root)?;
        let conversion = collect_linear_records_for_index_conversion(
            &old_root,
            block_size,
            self.superblock().inodes_count(),
        )?;

        let new_logical = directory
            .size()
            .checked_div(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        if new_logical != 1 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let allocation = self.allocate_block(None, handle)?;
        let new_physical = allocation.block();

        let mut root = indexed_root_block_bytes(
            block_size,
            directory.number(),
            conversion.parent,
            self.superblock().default_hash_version(),
            self.superblock().features().has_metadata_checksum(),
        )?;
        let root_count_limit =
            disk_dir::HTreeCountLimit::decode(&root, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET)?;
        self.update_htree_block_checksum(
            directory,
            &mut root,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            root_count_limit,
        )?;

        let mut leaf = vec![0; block_size];
        write_leaf_records(
            &mut leaf,
            &conversion.records,
            block_size,
            self.superblock().features().has_metadata_checksum(),
        )?;
        self.update_directory_block_checksum(directory, &mut leaf)?;

        let root_access = self
            .metadata_io
            .write_access(FilesystemBlock::new(root_physical.get()), handle)?;
        replace_metadata_access_bytes(&root_access, root)?;

        let leaf_access = self
            .metadata_io
            .create_access(FilesystemBlock::new(new_physical.get()), handle)?;
        replace_metadata_access_bytes(&leaf_access, leaf)?;

        self.insert_extent_mapping(
            directory,
            LogicalBlock::new(new_logical),
            new_physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        let new_size = directory
            .size()
            .checked_add(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        self.update_inode_size_metadata(directory, new_size, timestamp, handle)?;
        self.update_inode_flags_timestamps_metadata(
            directory,
            directory.flags() | disk_inode::EXT4_INDEX_FL,
            timestamp,
            handle,
        )?;
        self.insert_indexed_directory_entry(directory, name, inode, file_type, timestamp, handle)
    }

    fn remove_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<RemovedLinearDirectoryEntry> {
        if directory.has_indexed_directory() {
            return self.remove_indexed_directory_entry(directory, name, timestamp, handle);
        }
        self.remove_linear_directory_entry(directory, name, timestamp, handle)
    }

    fn remove_linear_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<RemovedLinearDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        if directory.size() == 0 || !directory.size().is_multiple_of(block_size_u64) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let block_count = directory
            .size()
            .checked_div(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;

        let mut block = vec![0; block_size];
        for logical in 0..block_count {
            let physical = match self.map_blocks(directory, LogicalBlock::new(logical))? {
                BlockMapping::Mapped { physical, len, .. }
                    if physical.get() != 0 && len.get() != 0 =>
                {
                    physical
                }
                BlockMapping::Mapped { .. } => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                }
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
                }
            };
            let read_len = self.read_directory_block_for_write(
                directory, logical, physical, block_size, &mut block,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if find_linear_remove_slot(&block, block_size, name, self.superblock().inodes_count())?
                .is_none()
            {
                continue;
            }

            let filesystem_block = FilesystemBlock::new(physical.get());
            let access = self.metadata_io.write_access(filesystem_block, handle)?;
            let mut bytes = metadata_access_bytes(&access)?;
            self.verify_directory_block(directory, logical, &bytes)?;
            let slot = find_linear_remove_slot(
                &bytes,
                block_size,
                name,
                self.superblock().inodes_count(),
            )?
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
            let removed = slot.remove(&mut bytes, block_size)?;
            self.update_directory_block_checksum(directory, &mut bytes)?;
            replace_metadata_access_bytes(&access, bytes)?;
            self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
            return Ok(RemovedLinearDirectoryEntry {
                inode: removed.inode,
                file_type: removed.file_type,
            });
        }

        Err(Ext4Error::NotFound)
    }

    fn insert_indexed_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let target = self.probe_htree_insert_target(directory, name, block_size)?;
        if self.insert_indexed_leaf_entry(
            directory,
            target.leaf_logical,
            target.leaf_physical,
            name,
            inode,
            file_type,
            handle,
        )? {
            return self.update_inode_timestamps_metadata(directory, timestamp, handle);
        }

        self.split_indexed_leaf_and_insert(
            directory, target, name, inode, file_type, timestamp, handle,
        )
    }

    fn remove_indexed_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<RemovedLinearDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut target = self.probe_htree_insert_target(directory, name, block_size)?;
        loop {
            match self.remove_indexed_leaf_entry(
                directory,
                target.leaf_logical,
                target.leaf_physical,
                name,
                timestamp,
                handle,
            ) {
                Ok(removed) => return Ok(removed),
                Err(Ext4Error::NotFound) => {
                    let Some(next) =
                        self.htree_next_lookup_leaf(directory, &mut target, block_size)?
                    else {
                        return Err(Ext4Error::NotFound);
                    };
                    target.leaf_logical = next.logical;
                    target.leaf_physical = next.physical;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn remove_indexed_leaf_entry(
        &mut self,
        directory: &Ext4Inode,
        logical: u64,
        physical: PhysicalBlock,
        name: &[u8],
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<RemovedLinearDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut block = vec![0; block_size];
        let read_len = self
            .read_directory_block_for_write(directory, logical, physical, block_size, &mut block)?;
        if read_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        if find_linear_remove_slot(&block, block_size, name, self.superblock().inodes_count())?
            .is_none()
        {
            return Err(Ext4Error::NotFound);
        }

        let filesystem_block = FilesystemBlock::new(physical.get());
        let access = self.metadata_io.write_access(filesystem_block, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        self.verify_directory_block(directory, logical, &bytes)?;
        let slot =
            find_linear_remove_slot(&bytes, block_size, name, self.superblock().inodes_count())?
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        let removed = slot.remove(&mut bytes, block_size)?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
        Ok(RemovedLinearDirectoryEntry {
            inode: removed.inode,
            file_type: removed.file_type,
        })
    }

    fn replace_indexed_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<ReplacedDirectoryEntry> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut target = self.probe_htree_insert_target(directory, name, block_size)?;
        loop {
            let mut block = vec![0; block_size];
            let read_len = self.read_directory_block_for_write(
                directory,
                target.leaf_logical,
                target.leaf_physical,
                block_size,
                &mut block,
            )?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            if find_linear_entry_slot(&block, block_size, name, self.superblock().inodes_count())?
                .is_some()
            {
                return self.replace_directory_entry_in_block(
                    directory,
                    target.leaf_logical,
                    target.leaf_physical,
                    name,
                    inode,
                    file_type,
                    timestamp,
                    handle,
                );
            }
            let Some(next) = self.htree_next_lookup_leaf(directory, &mut target, block_size)?
            else {
                return Err(Ext4Error::NotFound);
            };
            target.leaf_logical = next.logical;
            target.leaf_physical = next.physical;
        }
    }

    fn htree_next_lookup_leaf(
        &self,
        directory: &Ext4Inode,
        target: &mut HTreeInsertTarget,
        block_size: usize,
    ) -> Ext4Result<Option<HTreeLeafTarget>> {
        if self.htree_advance_frame(
            directory,
            &mut target.parent,
            block_size,
            target.hash.major(),
        )? {
            return self.htree_leaf_from_parent_frame(directory, &target.parent, block_size);
        }

        let Some(root) = target.root.as_mut() else {
            return Ok(None);
        };
        if !self.htree_advance_frame(directory, root, block_size, target.hash.major())? {
            return Ok(None);
        }

        target.parent = self.htree_first_node_frame_from_root(directory, root, block_size)?;
        self.htree_leaf_from_parent_frame(directory, &target.parent, block_size)
    }

    fn htree_advance_frame(
        &self,
        directory: &Ext4Inode,
        frame: &mut HTreeFrame,
        block_size: usize,
        hash: u32,
    ) -> Ext4Result<bool> {
        let next_index = frame
            .entry_index
            .checked_add(1)
            .ok_or(Ext4Error::Overflow)?;
        if next_index >= usize::from(frame.count_limit.count()) {
            return Ok(false);
        }

        let bytes = self.read_htree_frame_block(directory, frame, block_size)?;
        let entry = disk_dir::HTreeEntry::decode_indexed(&bytes, frame.count_offset, next_index)?;
        if entry.hash() & !1 != hash {
            return Ok(false);
        }
        frame.entry_index = next_index;
        Ok(true)
    }

    fn htree_leaf_from_parent_frame(
        &self,
        directory: &Ext4Inode,
        frame: &HTreeFrame,
        block_size: usize,
    ) -> Ext4Result<Option<HTreeLeafTarget>> {
        let logical = self.htree_frame_entry_block(directory, frame, block_size)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_count = directory_block_count_exact(directory.size(), block_size_u64)?;
        if logical == 0 || u64::from(logical) >= block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(Some(HTreeLeafTarget {
            logical: u64::from(logical),
            physical: self.mapped_directory_block(directory, u64::from(logical))?,
        }))
    }

    fn htree_first_node_frame_from_root(
        &self,
        directory: &Ext4Inode,
        root: &HTreeFrame,
        block_size: usize,
    ) -> Ext4Result<HTreeFrame> {
        let node_logical = u64::from(self.htree_frame_entry_block(directory, root, block_size)?);
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_count = directory_block_count_exact(directory.size(), block_size_u64)?;
        if node_logical == 0 || node_logical >= block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let node_physical = self.mapped_directory_block(directory, node_logical)?;
        let node = self.read_htree_block_bytes(node_physical, block_size)?;
        let node_count_limit =
            self.decode_htree_node_for_write(directory, node_logical, &node, block_size)?;
        if node_count_limit.count() == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(HTreeFrame {
            logical: node_logical,
            physical: node_physical,
            count_offset: disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            count_limit: node_count_limit,
            entry_index: 0,
        })
    }

    fn htree_frame_entry_block(
        &self,
        directory: &Ext4Inode,
        frame: &HTreeFrame,
        block_size: usize,
    ) -> Ext4Result<u32> {
        let bytes = self.read_htree_frame_block(directory, frame, block_size)?;
        htree_entry_block(&bytes, frame.count_offset, frame.entry_index)
    }

    fn read_htree_frame_block(
        &self,
        directory: &Ext4Inode,
        frame: &HTreeFrame,
        block_size: usize,
    ) -> Ext4Result<Vec<u8>> {
        let bytes = self.read_htree_block_bytes(frame.physical, block_size)?;
        self.verify_htree_block_checksum(
            directory,
            frame.logical,
            &bytes,
            frame.count_offset,
            frame.count_limit,
        )?;
        Ok(bytes)
    }

    fn read_htree_block_bytes(
        &self,
        physical: PhysicalBlock,
        block_size: usize,
    ) -> Ext4Result<Vec<u8>> {
        let mut bytes = vec![0; block_size];
        let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
        bytes.copy_from_slice(&buffer.as_ref()[..block_size]);
        Ok(bytes)
    }

    fn probe_htree_insert_target(
        &self,
        directory: &Ext4Inode,
        name: &[u8],
        block_size: usize,
    ) -> Ext4Result<HTreeInsertTarget> {
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_count = directory_block_count_exact(directory.size(), block_size_u64)?;
        if block_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let root_physical = self.mapped_directory_block(directory, 0)?;
        let mut root = vec![0; block_size];
        let root_buffer = self.read_metadata_block(FilesystemBlock::new(root_physical.get()))?;
        root.copy_from_slice(&root_buffer.as_ref()[..block_size]);
        let (root_info, root_count_limit) = self.decode_htree_root(&root, block_size)?;
        if root_info.indirect_levels() > 1 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
        }
        self.verify_htree_block_checksum(
            directory,
            0,
            &root,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            root_count_limit,
        )?;
        let hash = self.htree_hash(name, root_info.hash_version())?;
        let root_index = htree_select_entry(
            &root,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            root_count_limit,
            hash.major(),
        )?;
        let root_block =
            htree_entry_block(&root, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET, root_index)?;
        if root_block == 0 || u64::from(root_block) >= block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        if root_info.indirect_levels() == 0 {
            return Ok(HTreeInsertTarget {
                hash,
                hash_version: root_info.hash_version(),
                leaf_logical: u64::from(root_block),
                leaf_physical: self.mapped_directory_block(directory, u64::from(root_block))?,
                root: None,
                parent: HTreeFrame {
                    logical: 0,
                    physical: root_physical,
                    count_offset: disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
                    count_limit: root_count_limit,
                    entry_index: root_index,
                },
            });
        }
        if root_info.indirect_levels() > 1 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
        }

        let node_logical = u64::from(root_block);
        let node_physical = self.mapped_directory_block(directory, node_logical)?;
        let mut node = vec![0; block_size];
        let node_buffer = self.read_metadata_block(FilesystemBlock::new(node_physical.get()))?;
        node.copy_from_slice(&node_buffer.as_ref()[..block_size]);
        let node_count_limit =
            self.decode_htree_node_for_write(directory, node_logical, &node, block_size)?;
        self.verify_htree_block_checksum(
            directory,
            node_logical,
            &node,
            disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            node_count_limit,
        )?;
        let node_index = htree_select_entry(
            &node,
            disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            node_count_limit,
            hash.major(),
        )?;
        let leaf_block =
            htree_entry_block(&node, disk_dir::DX_NODE_COUNT_LIMIT_OFFSET, node_index)?;
        if leaf_block == 0 || u64::from(leaf_block) >= block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        Ok(HTreeInsertTarget {
            hash,
            hash_version: root_info.hash_version(),
            leaf_logical: u64::from(leaf_block),
            leaf_physical: self.mapped_directory_block(directory, u64::from(leaf_block))?,
            root: Some(HTreeFrame {
                logical: 0,
                physical: root_physical,
                count_offset: disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
                count_limit: root_count_limit,
                entry_index: root_index,
            }),
            parent: HTreeFrame {
                logical: node_logical,
                physical: node_physical,
                count_offset: disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
                count_limit: node_count_limit,
                entry_index: node_index,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_indexed_leaf_entry(
        &mut self,
        directory: &Ext4Inode,
        logical: u64,
        physical: PhysicalBlock,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<bool> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut block = vec![0; block_size];
        let read_len = self
            .read_directory_block_for_write(directory, logical, physical, block_size, &mut block)?;
        if read_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        if find_linear_insert_slot(&block, block_size, name.len())?.is_none() {
            return Ok(false);
        }

        let access = self
            .metadata_io
            .write_access(FilesystemBlock::new(physical.get()), handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        self.verify_directory_block(directory, logical, &bytes)?;
        let slot = find_linear_insert_slot(&bytes, block_size, name.len())?
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        slot.write(&mut bytes, block_size, inode, file_type, name)?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn split_indexed_leaf_and_insert(
        &mut self,
        directory: &Ext4Inode,
        target: HTreeInsertTarget,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if usize::from(target.parent.count_limit.count())
            >= usize::from(target.parent.count_limit.limit())
        {
            return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
        }

        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let mut old_bytes = vec![0; block_size];
        let read_len = self.read_directory_block_for_write(
            directory,
            target.leaf_logical,
            target.leaf_physical,
            block_size,
            &mut old_bytes,
        )?;
        if read_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        let mut records =
            self.collect_leaf_records_for_split(&old_bytes, block_size, target.hash_version)?;
        records.sort_by_key(|record| record.hash);
        let split = htree_leaf_split_index(&records, block_size)?;
        let hash2 = records
            .get(split)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?
            .hash;
        let continued = records
            .get(split.checked_sub(1).ok_or(Ext4Error::Overflow)?)
            .is_some_and(|record| record.hash == hash2);
        let index_hash = hash2.wrapping_add(u32::from(continued));

        let mut left_bytes = vec![0; block_size];
        let mut right_bytes = vec![0; block_size];
        write_leaf_records(
            &mut left_bytes,
            &records[..split],
            block_size,
            self.superblock().features().has_metadata_checksum(),
        )?;
        write_leaf_records(
            &mut right_bytes,
            &records[split..],
            block_size,
            self.superblock().features().has_metadata_checksum(),
        )?;

        let new_logical = directory
            .size()
            .checked_div(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;

        if target.hash.major() >= hash2 {
            let slot = find_linear_insert_slot(&right_bytes, block_size, name.len())?
                .ok_or(Ext4Error::NoSpace)?;
            slot.write(&mut right_bytes, block_size, inode, file_type, name)?;
        } else {
            let slot = find_linear_insert_slot(&left_bytes, block_size, name.len())?
                .ok_or(Ext4Error::NoSpace)?;
            slot.write(&mut left_bytes, block_size, inode, file_type, name)?;
        }
        self.update_directory_block_checksum(directory, &mut left_bytes)?;
        self.update_directory_block_checksum(directory, &mut right_bytes)?;
        let allocation = self.allocate_block(None, handle)?;
        let new_physical = allocation.block();

        let old_access = self
            .metadata_io
            .write_access(FilesystemBlock::new(target.leaf_physical.get()), handle)?;
        replace_metadata_access_bytes(&old_access, left_bytes)?;

        let new_access = self
            .metadata_io
            .create_access(FilesystemBlock::new(new_physical.get()), handle)?;
        replace_metadata_access_bytes(&new_access, right_bytes)?;

        self.insert_extent_mapping(
            directory,
            LogicalBlock::new(new_logical),
            new_physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        let new_size = directory
            .size()
            .checked_add(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        self.update_inode_size_metadata(directory, new_size, timestamp, handle)?;

        let parent_access = self
            .metadata_io
            .write_access(FilesystemBlock::new(target.parent.physical.get()), handle)?;
        let mut parent_bytes = metadata_access_bytes(&parent_access)?;
        self.verify_htree_block_checksum(
            directory,
            target.parent.logical,
            &parent_bytes,
            target.parent.count_offset,
            target.parent.count_limit,
        )?;
        let count_limit = insert_htree_index_entry(
            &mut parent_bytes,
            target.parent.count_offset,
            target.parent.count_limit,
            target.parent.entry_index,
            index_hash,
            u32::try_from(new_logical).map_err(|_| Ext4Error::Overflow)?,
        )?;
        self.update_htree_block_checksum(
            directory,
            &mut parent_bytes,
            target.parent.count_offset,
            count_limit,
        )?;
        replace_metadata_access_bytes(&parent_access, parent_bytes)?;
        self.update_inode_timestamps_metadata(directory, timestamp, handle)
    }

    fn decode_htree_node_for_write(
        &self,
        directory: &Ext4Inode,
        logical_block: u64,
        bytes: &[u8],
        block_size: usize,
    ) -> Ext4Result<disk_dir::HTreeCountLimit> {
        let fake = disk_dir::RawDirectoryEntry::decode(bytes, 0)?;
        let fake_len = fake.record_len(block_size)?;
        if fake.inode() != 0 || fake.name_len() != 0 || fake_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let count_limit =
            self.decode_htree_count_limit(bytes, disk_dir::DX_NODE_COUNT_LIMIT_OFFSET, block_size)?;
        self.verify_htree_block_checksum(
            directory,
            logical_block,
            bytes,
            disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            count_limit,
        )?;
        Ok(count_limit)
    }

    fn update_htree_block_checksum(
        &self,
        inode: &Ext4Inode,
        bytes: &mut [u8],
        count_offset: usize,
        count_limit: disk_dir::HTreeCountLimit,
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        let limit = usize::from(count_limit.limit());
        let count = usize::from(count_limit.count());
        let tail_offset = count_offset
            .checked_add(
                limit
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let used_len = count_offset
            .checked_add(
                count
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let checksum_offset = tail_offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
        let tail_reserved = bytes
            .get(tail_offset..checksum_offset)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let mut checksum = checksum::crc32c(
            inode_checksum_seed(
                self.superblock().checksum_seed(),
                inode.number(),
                inode.generation(),
            ),
            bytes
                .get(..used_len)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?,
        );
        checksum = checksum::crc32c(checksum, tail_reserved);
        checksum = checksum::crc32c(checksum, &0u32.to_le_bytes());
        put_u32(bytes, checksum_offset, checksum)
    }

    fn mapped_directory_block(
        &self,
        directory: &Ext4Inode,
        logical: u64,
    ) -> Ext4Result<PhysicalBlock> {
        match self.map_blocks(directory, LogicalBlock::new(logical))? {
            BlockMapping::Mapped { physical, len, .. } if physical.get() != 0 && len.get() != 0 => {
                Ok(physical)
            }
            BlockMapping::Mapped { .. } => Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent)),
            BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))
            }
        }
    }

    fn read_directory_block_for_write(
        &self,
        directory: &Ext4Inode,
        logical: u64,
        physical: PhysicalBlock,
        block_size: usize,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        if output.len() < block_size {
            return Err(Ext4Error::InvalidBufferLength {
                expected: block_size,
                actual: output.len(),
            });
        }
        let output = &mut output[..block_size];
        let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
        output.copy_from_slice(&buffer.as_ref()[..block_size]);
        self.verify_directory_block(directory, logical, output)?;
        Ok(block_size)
    }

    fn update_directory_block_checksum(
        &self,
        inode: &Ext4Inode,
        bytes: &mut [u8],
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        let tail_offset = bytes
            .len()
            .checked_sub(disk_dir::DIRENT_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let tail = disk_dir::RawDirectoryEntry::decode(bytes, tail_offset)?;
        if !tail.is_checksum_tail() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let checksum_seed = inode_checksum_seed(
            self.superblock().checksum_seed(),
            inode.number(),
            inode.generation(),
        );
        let checksum = checksum::crc32c(
            checksum_seed,
            bytes
                .get(..tail_offset)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?,
        );
        let checksum_offset = bytes.len() - 4;
        put_u32(bytes, checksum_offset, checksum)
    }

    fn append_linear_directory_entry(
        &mut self,
        directory: &Ext4Inode,
        name: &[u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let logical = directory
            .size()
            .checked_div(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        let allocation = self.allocate_block(None, handle)?;
        let block = FilesystemBlock::new(allocation.block().get());
        let access = self.metadata_io.create_access(block, handle)?;
        let mut bytes = vec![0; block_size];
        let dirent_len = if self.superblock().features().has_metadata_checksum() {
            let tail_offset = block_size
                .checked_sub(disk_dir::DIRENT_TAIL_SIZE)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            write_checksum_tail(&mut bytes, tail_offset)?;
            tail_offset
        } else {
            block_size
        };
        write_dirent(
            &mut bytes, 0, dirent_len, block_size, inode, file_type, name,
        )?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        self.insert_extent_mapping(
            directory,
            LogicalBlock::new(logical),
            allocation.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        self.update_inode_size_metadata(
            directory,
            directory
                .size()
                .checked_add(block_size_u64)
                .ok_or(Ext4Error::Overflow)?,
            timestamp,
            handle,
        )
    }

    fn namei_metadata_journal(
        &mut self,
        credits: JournalCredits,
    ) -> Ext4Result<Arc<MountedJournal>> {
        self.namei_metadata_journal_with_policy(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn namei_metadata_journal_with_policy(
        &mut self,
        credits: JournalCredits,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<Arc<MountedJournal>> {
        self.metadata_journal_for_mutation(credits, recovery_flag_policy)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemovedLinearDirectoryEntry {
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplacedDirectoryEntry {
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HTreeFrame {
    logical: u64,
    physical: PhysicalBlock,
    count_offset: usize,
    count_limit: disk_dir::HTreeCountLimit,
    entry_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HTreeInsertTarget {
    hash: DirectoryHash,
    hash_version: u8,
    leaf_logical: u64,
    leaf_physical: PhysicalBlock,
    root: Option<HTreeFrame>,
    parent: HTreeFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HTreeLeafTarget {
    logical: u64,
    physical: PhysicalBlock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryLeafRecord {
    hash: u32,
    inode: InodeNumber,
    file_type: DirectoryFileType,
    name: Vec<u8>,
    rec_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IndexConversionRecords {
    parent: InodeNumber,
    records: Vec<DirectoryLeafRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemovedDirectoryRecord {
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinearEntrySlot {
    offset: usize,
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

impl LinearEntrySlot {
    fn replace(
        self,
        bytes: &mut [u8],
        inode: InodeNumber,
        file_type: DirectoryFileType,
    ) -> Ext4Result<RemovedDirectoryRecord> {
        put_u32(bytes, self.offset, inode.get())?;
        let file_type_byte = bytes
            .get_mut(self.offset.checked_add(7).ok_or(Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        *file_type_byte = file_type.to_raw();
        Ok(RemovedDirectoryRecord {
            inode: self.inode,
            file_type: self.file_type,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinearInsertSlot {
    Free {
        offset: usize,
        rec_len: usize,
    },
    Split {
        existing_offset: usize,
        existing_rec_len: usize,
        new_offset: usize,
        new_rec_len: usize,
    },
}

impl LinearInsertSlot {
    fn write(
        self,
        bytes: &mut [u8],
        block_size: usize,
        inode: InodeNumber,
        file_type: DirectoryFileType,
        name: &[u8],
    ) -> Ext4Result<()> {
        match self {
            Self::Free { offset, rec_len } => {
                let needed = dirent_record_len(name.len())?;
                if rec_len >= needed + disk_dir::DIRENT_HEADER_SIZE {
                    write_dirent(bytes, offset, needed, block_size, inode, file_type, name)?;
                    write_free_dirent(bytes, offset + needed, rec_len - needed, block_size)
                } else {
                    write_dirent(bytes, offset, rec_len, block_size, inode, file_type, name)
                }
            }
            Self::Split {
                existing_offset,
                existing_rec_len,
                new_offset,
                new_rec_len,
            } => {
                put_u16(
                    bytes,
                    existing_offset + 4,
                    disk_dir::RawDirectoryEntry::encode_record_len(existing_rec_len, block_size)?,
                )?;
                write_dirent(
                    bytes,
                    new_offset,
                    new_rec_len,
                    block_size,
                    inode,
                    file_type,
                    name,
                )
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinearRemoveSlot {
    previous: Option<(usize, usize)>,
    target_offset: usize,
    target_rec_len: usize,
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

impl LinearRemoveSlot {
    fn remove(self, bytes: &mut [u8], block_size: usize) -> Ext4Result<RemovedDirectoryRecord> {
        match self.previous {
            Some((previous_offset, previous_rec_len)) => {
                let merged_len = previous_rec_len
                    .checked_add(self.target_rec_len)
                    .ok_or(Ext4Error::Overflow)?;
                put_u16(
                    bytes,
                    previous_offset + 4,
                    disk_dir::RawDirectoryEntry::encode_record_len(merged_len, block_size)?,
                )?;
                let target_end = self
                    .target_offset
                    .checked_add(self.target_rec_len)
                    .ok_or(Ext4Error::Overflow)?;
                bytes
                    .get_mut(self.target_offset..target_end)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
                    .fill(0);
            }
            None => {
                write_free_dirent(bytes, self.target_offset, self.target_rec_len, block_size)?;
            }
        }
        Ok(RemovedDirectoryRecord {
            inode: self.inode,
            file_type: self.file_type,
        })
    }
}

fn find_linear_insert_slot(
    bytes: &[u8],
    block_size: usize,
    name_len: usize,
) -> Ext4Result<Option<LinearInsertSlot>> {
    let needed = dirent_record_len(name_len)?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = entry.record_len(block_size)?;
        if rec_len == 0 || !rec_len.is_multiple_of(4) || rec_len < disk_dir::DIRENT_HEADER_SIZE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.is_checksum_tail() {
            if next != bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            return Ok(None);
        }

        let entry_name_len = usize::from(entry.name_len());
        if entry_name_len > disk_dir::DIRENT_NAME_MAX
            || disk_dir::DIRENT_HEADER_SIZE
                .checked_add(entry_name_len)
                .ok_or(Ext4Error::Overflow)?
                > rec_len
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        if entry.inode() == 0 {
            if rec_len >= needed {
                return Ok(Some(LinearInsertSlot::Free { offset, rec_len }));
            }
        } else {
            let existing_rec_len = dirent_record_len(entry_name_len)?;
            if rec_len >= existing_rec_len + needed {
                return Ok(Some(LinearInsertSlot::Split {
                    existing_offset: offset,
                    existing_rec_len,
                    new_offset: offset + existing_rec_len,
                    new_rec_len: rec_len - existing_rec_len,
                }));
            }
        }

        offset = next;
    }

    Ok(None)
}

fn find_linear_remove_slot(
    bytes: &[u8],
    block_size: usize,
    name: &[u8],
    inodes_count: u32,
) -> Ext4Result<Option<LinearRemoveSlot>> {
    let mut offset = 0usize;
    let mut previous = None;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = entry.record_len(block_size)?;
        if rec_len == 0 || !rec_len.is_multiple_of(4) || rec_len < disk_dir::DIRENT_HEADER_SIZE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.is_checksum_tail() {
            if next != bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            return Ok(None);
        }

        let entry_name_len = usize::from(entry.name_len());
        if entry_name_len > disk_dir::DIRENT_NAME_MAX
            || disk_dir::DIRENT_HEADER_SIZE
                .checked_add(entry_name_len)
                .ok_or(Ext4Error::Overflow)?
                > rec_len
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        if entry.inode() != 0 {
            if entry.inode() > inodes_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let name_start = offset
                .checked_add(disk_dir::DIRENT_HEADER_SIZE)
                .ok_or(Ext4Error::Overflow)?;
            let name_end = name_start
                .checked_add(entry_name_len)
                .ok_or(Ext4Error::Overflow)?;
            let entry_name = bytes
                .get(name_start..name_end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            if entry_name == name {
                return Ok(Some(LinearRemoveSlot {
                    previous,
                    target_offset: offset,
                    target_rec_len: rec_len,
                    inode: InodeNumber::new(entry.inode()),
                    file_type: entry.file_type(),
                }));
            }
        }

        previous = Some((offset, rec_len));
        offset = next;
    }

    Ok(None)
}

fn find_linear_entry_slot(
    bytes: &[u8],
    block_size: usize,
    name: &[u8],
    inodes_count: u32,
) -> Ext4Result<Option<LinearEntrySlot>> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = entry.record_len(block_size)?;
        if rec_len == 0 || !rec_len.is_multiple_of(4) || rec_len < disk_dir::DIRENT_HEADER_SIZE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.is_checksum_tail() {
            if next != bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            return Ok(None);
        }
        let entry_name_len = usize::from(entry.name_len());
        if entry_name_len > disk_dir::DIRENT_NAME_MAX
            || disk_dir::DIRENT_HEADER_SIZE
                .checked_add(entry_name_len)
                .ok_or(Ext4Error::Overflow)?
                > rec_len
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.inode() != 0 {
            if entry.inode() > inodes_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let name_start = offset
                .checked_add(disk_dir::DIRENT_HEADER_SIZE)
                .ok_or(Ext4Error::Overflow)?;
            let name_end = name_start
                .checked_add(entry_name_len)
                .ok_or(Ext4Error::Overflow)?;
            let entry_name = bytes
                .get(name_start..name_end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            if entry_name == name {
                return Ok(Some(LinearEntrySlot {
                    offset,
                    inode: InodeNumber::new(entry.inode()),
                    file_type: entry.file_type(),
                }));
            }
        }
        offset = next;
    }
    Ok(None)
}

fn directory_block_count_exact(size: u64, block_size: u64) -> Ext4Result<u64> {
    if size == 0 || !size.is_multiple_of(block_size) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
    }
    size.checked_div(block_size).ok_or(Ext4Error::Overflow)
}

fn htree_select_entry(
    bytes: &[u8],
    count_offset: usize,
    count_limit: disk_dir::HTreeCountLimit,
    hash: u32,
) -> Ext4Result<usize> {
    let count = usize::from(count_limit.count());
    if count == 0 || count > usize::from(count_limit.limit()) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
    }
    let mut selected = 0usize;
    for index in 1..count {
        let entry = disk_dir::HTreeEntry::decode_indexed(bytes, count_offset, index)?;
        if entry.hash() > hash {
            break;
        }
        selected = index;
    }
    Ok(selected)
}

fn htree_entry_block(bytes: &[u8], count_offset: usize, index: usize) -> Ext4Result<u32> {
    Ok(disk_dir::HTreeEntry::decode_indexed(bytes, count_offset, index)?.block())
}

fn insert_htree_index_entry(
    bytes: &mut [u8],
    count_offset: usize,
    count_limit: disk_dir::HTreeCountLimit,
    after_index: usize,
    hash: u32,
    block: u32,
) -> Ext4Result<disk_dir::HTreeCountLimit> {
    let count = usize::from(count_limit.count());
    let limit = usize::from(count_limit.limit());
    if count == 0 || count >= limit || after_index >= count {
        return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
    }
    let new_index = after_index.checked_add(1).ok_or(Ext4Error::Overflow)?;
    let move_start = count_offset
        .checked_add(
            new_index
                .checked_mul(disk_dir::DX_ENTRY_SIZE)
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    let move_end = count_offset
        .checked_add(
            count
                .checked_mul(disk_dir::DX_ENTRY_SIZE)
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    let move_dest = move_start
        .checked_add(disk_dir::DX_ENTRY_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    if move_end > bytes.len() || move_dest > bytes.len() {
        return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
    }
    bytes.copy_within(move_start..move_end, move_dest);
    let entry_offset = count_offset
        .checked_add(
            new_index
                .checked_mul(disk_dir::DX_ENTRY_SIZE)
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    put_u32(bytes, entry_offset, hash)?;
    put_u32(bytes, entry_offset + 4, block)?;
    put_u16(
        bytes,
        count_offset + 2,
        u16::try_from(count + 1).map_err(|_| Ext4Error::Overflow)?,
    )?;
    disk_dir::HTreeCountLimit::decode(bytes, count_offset)
}

impl Ext4SbInfo {
    fn collect_leaf_records_for_split(
        &self,
        bytes: &[u8],
        block_size: usize,
        hash_version: u8,
    ) -> Ext4Result<Vec<DirectoryLeafRecord>> {
        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
            let rec_len = entry.record_len(block_size)?;
            if rec_len == 0 || !rec_len.is_multiple_of(4) || rec_len < disk_dir::DIRENT_HEADER_SIZE
            {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
            if next > bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            if entry.is_checksum_tail() {
                if next != bytes.len() {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
                }
                break;
            }
            let name_len = usize::from(entry.name_len());
            if name_len > disk_dir::DIRENT_NAME_MAX
                || disk_dir::DIRENT_HEADER_SIZE
                    .checked_add(name_len)
                    .ok_or(Ext4Error::Overflow)?
                    > rec_len
            {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            if entry.inode() != 0 {
                if entry.inode() > self.superblock().inodes_count() {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
                }
                let name_start = offset
                    .checked_add(disk_dir::DIRENT_HEADER_SIZE)
                    .ok_or(Ext4Error::Overflow)?;
                let name_end = name_start
                    .checked_add(name_len)
                    .ok_or(Ext4Error::Overflow)?;
                let name = bytes
                    .get(name_start..name_end)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                records.push(DirectoryLeafRecord {
                    hash: self.htree_hash(name, hash_version)?.major(),
                    inode: InodeNumber::new(entry.inode()),
                    file_type: entry.file_type(),
                    name: Vec::from(name),
                    rec_len,
                });
            }
            offset = next;
        }
        Ok(records)
    }
}

fn collect_linear_records_for_index_conversion(
    bytes: &[u8],
    block_size: usize,
    inodes_count: u32,
) -> Ext4Result<IndexConversionRecords> {
    let mut records = Vec::new();
    let mut parent = None;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = entry.record_len(block_size)?;
        if rec_len == 0 || !rec_len.is_multiple_of(4) || rec_len < disk_dir::DIRENT_HEADER_SIZE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.is_checksum_tail() {
            if next != bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            break;
        }
        let name_len = usize::from(entry.name_len());
        if name_len > disk_dir::DIRENT_NAME_MAX
            || disk_dir::DIRENT_HEADER_SIZE
                .checked_add(name_len)
                .ok_or(Ext4Error::Overflow)?
                > rec_len
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.inode() != 0 {
            if entry.inode() > inodes_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let name_start = offset
                .checked_add(disk_dir::DIRENT_HEADER_SIZE)
                .ok_or(Ext4Error::Overflow)?;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or(Ext4Error::Overflow)?;
            let name = bytes
                .get(name_start..name_end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            match name {
                b"." => {}
                b".." => parent = Some(InodeNumber::new(entry.inode())),
                _ => records.push(DirectoryLeafRecord {
                    hash: 0,
                    inode: InodeNumber::new(entry.inode()),
                    file_type: entry.file_type(),
                    name: Vec::from(name),
                    rec_len,
                }),
            }
        }
        offset = next;
    }
    Ok(IndexConversionRecords {
        parent: parent.ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?,
        records,
    })
}

fn htree_leaf_split_index(records: &[DirectoryLeafRecord], block_size: usize) -> Ext4Result<usize> {
    if records.len() < 2 {
        return Err(Ext4Error::NoSpace);
    }
    let mut size = 0usize;
    let mut weighted_split = None;
    for (moved, index) in (0..records.len()).rev().enumerate() {
        if size
            .checked_add(records[index].rec_len / 2)
            .ok_or(Ext4Error::Overflow)?
            > block_size / 2
        {
            weighted_split = Some(records.len() - moved);
            break;
        }
        size = size
            .checked_add(records[index].rec_len)
            .ok_or(Ext4Error::Overflow)?;
    }
    let split = weighted_split.unwrap_or(records.len() / 2);
    if split == 0 || split >= records.len() {
        return Err(Ext4Error::NoSpace);
    }
    Ok(split)
}

fn write_leaf_records(
    bytes: &mut [u8],
    records: &[DirectoryLeafRecord],
    block_size: usize,
    has_metadata_checksum: bool,
) -> Ext4Result<()> {
    bytes.fill(0);
    let usable_len = if has_metadata_checksum {
        let tail_offset = block_size
            .checked_sub(disk_dir::DIRENT_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        write_checksum_tail(bytes, tail_offset)?;
        tail_offset
    } else {
        block_size
    };
    if records.is_empty() {
        return write_free_dirent(bytes, 0, usable_len, block_size);
    }

    let mut offset = 0usize;
    for (index, record) in records.iter().enumerate() {
        let rec_len = if index + 1 == records.len() {
            usable_len.checked_sub(offset).ok_or(Ext4Error::Overflow)?
        } else {
            dirent_record_len(record.name.len())?
        };
        write_dirent(
            bytes,
            offset,
            rec_len,
            block_size,
            record.inode,
            record.file_type,
            &record.name,
        )?;
        offset = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
    }
    Ok(())
}

fn indexed_root_block_bytes(
    block_size: usize,
    directory: InodeNumber,
    parent: InodeNumber,
    hash_version: u8,
    has_metadata_checksum: bool,
) -> Ext4Result<Vec<u8>> {
    let mut bytes = vec![0; block_size];
    write_dirent(
        &mut bytes,
        0,
        dirent_record_len(1)?,
        block_size,
        directory,
        DirectoryFileType::Directory,
        b".",
    )?;
    write_dirent(
        &mut bytes,
        dirent_record_len(1)?,
        block_size
            .checked_sub(dirent_record_len(1)?)
            .ok_or(Ext4Error::Overflow)?,
        block_size,
        parent,
        DirectoryFileType::Directory,
        b"..",
    )?;
    put_u32(&mut bytes, disk_dir::DX_ROOT_INFO_OFFSET, 0)?;
    bytes[disk_dir::DX_ROOT_INFO_OFFSET + 4] = hash_version;
    bytes[disk_dir::DX_ROOT_INFO_OFFSET + 5] = 8;
    bytes[disk_dir::DX_ROOT_INFO_OFFSET + 6] = 0;
    bytes[disk_dir::DX_ROOT_INFO_OFFSET + 7] = 0;
    let tail_size = if has_metadata_checksum {
        disk_dir::DX_TAIL_SIZE
    } else {
        0
    };
    let limit = block_size
        .checked_sub(disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET)
        .and_then(|space| space.checked_sub(tail_size))
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?
        / disk_dir::DX_ENTRY_SIZE;
    put_u16(
        &mut bytes,
        disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
        u16::try_from(limit).map_err(|_| Ext4Error::Overflow)?,
    )?;
    put_u16(&mut bytes, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET + 2, 1)?;
    put_u32(&mut bytes, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET + 4, 1)?;
    if has_metadata_checksum {
        let tail_offset = disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET
            .checked_add(
                limit
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        put_u32(&mut bytes, tail_offset, 0)?;
        put_u32(&mut bytes, tail_offset + 4, 0)?;
    }
    Ok(bytes)
}

fn initial_directory_block_bytes(
    block_size: usize,
    directory: InodeNumber,
    parent: InodeNumber,
    has_metadata_checksum: bool,
) -> Ext4Result<alloc::vec::Vec<u8>> {
    let mut bytes = vec![0; block_size];
    let dot_len = dirent_record_len(1)?;
    let usable_len = if has_metadata_checksum {
        block_size
            .checked_sub(disk_dir::DIRENT_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
    } else {
        block_size
    };
    let dotdot_len = usable_len.checked_sub(dot_len).ok_or(Ext4Error::Overflow)?;
    write_dirent(
        &mut bytes,
        0,
        dot_len,
        block_size,
        directory,
        DirectoryFileType::Directory,
        b".",
    )?;
    write_dirent(
        &mut bytes,
        dot_len,
        dotdot_len,
        block_size,
        parent,
        DirectoryFileType::Directory,
        b"..",
    )?;
    if has_metadata_checksum {
        write_checksum_tail(&mut bytes, usable_len)?;
    }
    Ok(bytes)
}

fn validate_new_entry_name(name: &[u8]) -> Ext4Result<()> {
    if name.is_empty()
        || name.len() > disk_dir::DIRENT_NAME_MAX
        || name == b"."
        || name == b".."
        || name.iter().any(|byte| *byte == 0 || *byte == b'/')
    {
        return Err(Ext4Error::InvalidName);
    }
    Ok(())
}

fn validate_symlink_target(target: &[u8]) -> Ext4Result<()> {
    if target.is_empty() || target.contains(&0) {
        return Err(Ext4Error::InvalidName);
    }
    Ok(())
}

const fn directory_file_type_for_inode_kind(kind: InodeKind) -> DirectoryFileType {
    match kind {
        InodeKind::Fifo => DirectoryFileType::Fifo,
        InodeKind::CharacterDevice => DirectoryFileType::CharacterDevice,
        InodeKind::Directory => DirectoryFileType::Directory,
        InodeKind::BlockDevice => DirectoryFileType::BlockDevice,
        InodeKind::RegularFile => DirectoryFileType::RegularFile,
        InodeKind::Symlink => DirectoryFileType::Symlink,
        InodeKind::Socket => DirectoryFileType::Socket,
    }
}

fn dirent_record_len(name_len: usize) -> Ext4Result<usize> {
    disk_dir::DIRENT_HEADER_SIZE
        .checked_add(name_len)
        .and_then(|len| len.checked_add(3))
        .map(|len| len & !3)
        .ok_or(Ext4Error::Overflow)
}

fn write_dirent(
    bytes: &mut [u8],
    offset: usize,
    rec_len: usize,
    block_size: usize,
    inode: InodeNumber,
    file_type: DirectoryFileType,
    name: &[u8],
) -> Ext4Result<()> {
    let needed = dirent_record_len(name.len())?;
    if rec_len < needed || name.len() > disk_dir::DIRENT_NAME_MAX {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
    }
    let end = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
    let record = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    record.fill(0);
    put_u32(record, 0, inode.get())?;
    put_u16(
        record,
        4,
        disk_dir::RawDirectoryEntry::encode_record_len(rec_len, block_size)?,
    )?;
    record[6] = u8::try_from(name.len()).map_err(|_| Ext4Error::InvalidName)?;
    record[7] = file_type.to_raw();
    let name_end = disk_dir::DIRENT_HEADER_SIZE
        .checked_add(name.len())
        .ok_or(Ext4Error::Overflow)?;
    record
        .get_mut(disk_dir::DIRENT_HEADER_SIZE..name_end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(name);
    Ok(())
}

fn write_free_dirent(
    bytes: &mut [u8],
    offset: usize,
    rec_len: usize,
    block_size: usize,
) -> Ext4Result<()> {
    if rec_len < disk_dir::DIRENT_HEADER_SIZE {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
    }
    let end = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
    let record = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    record.fill(0);
    put_u16(
        record,
        4,
        disk_dir::RawDirectoryEntry::encode_record_len(rec_len, block_size)?,
    )
}

fn write_checksum_tail(bytes: &mut [u8], offset: usize) -> Ext4Result<()> {
    let end = offset
        .checked_add(disk_dir::DIRENT_TAIL_SIZE)
        .ok_or(Ext4Error::Overflow)?;
    let record = bytes
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    record.fill(0);
    put_u16(record, 4, disk_dir::DIRENT_TAIL_SIZE as u16)?;
    record[7] = disk_dir::DIRENT_TAIL_FILE_TYPE;
    Ok(())
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = offset.checked_add(2).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}
