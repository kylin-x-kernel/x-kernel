// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style namespace mutation helpers for ext4 directories.

use alloc::{vec, vec::Vec};

use crate::{
    BlockCount, BlockMapping, CorruptKind, Ext4Error, Ext4Filesystem, Ext4Result, FilesystemBlock,
    InodeNumber, LogicalBlock, PhysicalBlock, UnsupportedKind,
    dirhash::{DirectoryHash, directory_hash},
    disk::{DirectoryFileType, checksum, dir as disk_dir, inode as disk_inode},
    extent::ExtentMappingState,
    inode::{
        Ext4DeviceId, Ext4Inode, Ext4Timestamp, InodeInitialization, InodeKind, inode_checksum_seed,
    },
    jbd2::{Journal, JournalCredits, TransactionId},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    superblock::{metadata_access_bytes, replace_metadata_access_bytes},
    xattr::external_xattr_eviction_credits,
};

const EXT4_LINK_MAX: u16 = 65_000;

struct Ext4Credits;

impl Ext4Credits {
    const DIRECTORY_BLOCK_GROW: u32 = 4;
    const DIRECTORY_BLOCK_UPDATE: u32 = 1;
    const EXTENT_TREE_DELETE_CEILING: u32 = 4096;
    const FILE_BLOCK_ALLOCATOR: u32 = 8;
    const FILE_EXTENT_UPDATE: u32 = 8;
    const HTREE_INDEX_EXTRA: u32 = 8;
    const INODE_ALLOCATOR: u32 = 8;
    const INODE_FREE: u32 = 8;
    const INODE_UPDATE: u32 = 1;
    const ORPHAN_LINK_UPDATE: u32 = 2;

    fn create(_filesystem: &Ext4Filesystem, directory: &Ext4Inode) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR + Self::dirent_update(directory) + Self::DIRECTORY_BLOCK_GROW,
        )
    }

    fn mkdir(_filesystem: &Ext4Filesystem, directory: &Ext4Inode) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR
                + Self::DIRECTORY_BLOCK_UPDATE
                + Self::dirent_update(directory)
                + Self::DIRECTORY_BLOCK_GROW
                + Self::FILE_BLOCK_ALLOCATOR
                + Self::FILE_EXTENT_UPDATE
                + Self::INODE_UPDATE,
        )
    }

    fn block_mapped_symlink(_filesystem: &Ext4Filesystem, directory: &Ext4Inode) -> JournalCredits {
        Self::credits(
            Self::INODE_ALLOCATOR
                + Self::DIRECTORY_BLOCK_UPDATE
                + Self::dirent_update(directory)
                + Self::DIRECTORY_BLOCK_GROW
                + Self::INODE_UPDATE,
        )
    }

    fn link(
        _filesystem: &Ext4Filesystem,
        directory: &Ext4Inode,
        _target: &Ext4Inode,
    ) -> JournalCredits {
        Self::credits(Self::INODE_UPDATE + Self::dirent_update(directory))
    }

    fn unlink(
        _filesystem: &Ext4Filesystem,
        directory: &Ext4Inode,
        victim: &Ext4Inode,
    ) -> JournalCredits {
        let zero_link = if victim.links_count() == 1 {
            Self::zero_link_eviction(victim)
        } else {
            Self::INODE_UPDATE
        };
        Self::credits(Self::dirent_update(directory) + zero_link)
    }

    fn rmdir(
        _filesystem: &Ext4Filesystem,
        directory: &Ext4Inode,
        victim: &Ext4Inode,
    ) -> JournalCredits {
        Self::credits(
            Self::dirent_update(directory) + Self::INODE_UPDATE + Self::zero_link_eviction(victim),
        )
    }

    fn rename(
        _filesystem: &Ext4Filesystem,
        old_directory: &Ext4Inode,
        new_directory: &Ext4Inode,
        moved: &Ext4Inode,
        replaced: Option<&Ext4Inode>,
    ) -> JournalCredits {
        let parent_link_updates = if moved.kind() == InodeKind::Directory {
            Self::INODE_UPDATE * 2
        } else {
            0
        };
        let replaced_update = replaced.map_or(0, |victim| {
            if victim.links_count() == 1 || victim.kind() == InodeKind::Directory {
                Self::zero_link_eviction(victim)
            } else {
                Self::INODE_UPDATE
            }
        });
        Self::credits(
            Self::dirent_update(new_directory)
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

    fn zero_link_eviction(inode: &Ext4Inode) -> u32 {
        let extent_delete = if inode.blocks() == 0 {
            0
        } else {
            Self::EXTENT_TREE_DELETE_CEILING
        };
        Self::ORPHAN_LINK_UPDATE
            + Self::INODE_UPDATE
            + Self::INODE_FREE
            + external_xattr_eviction_credits(inode)
            + extent_delete
    }
}

/// Result of creating a namespace entry in one parent directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4NamespaceCreate {
    parent: Ext4Inode,
    child: Ext4Inode,
}

impl Ext4NamespaceCreate {
    /// Returns the updated parent directory inode.
    pub const fn parent(&self) -> &Ext4Inode {
        &self.parent
    }

    /// Returns the newly allocated child inode.
    pub const fn child(&self) -> &Ext4Inode {
        &self.child
    }
}

/// Result of removing one namespace entry from a parent directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4NamespaceRemove {
    parent: Ext4Inode,
    removed: Ext4Inode,
    removed_file_type: DirectoryFileType,
}

impl Ext4NamespaceRemove {
    /// Returns the updated parent directory inode.
    pub const fn parent(&self) -> &Ext4Inode {
        &self.parent
    }

    /// Returns the inode number that was unlinked from the directory.
    pub const fn removed_inode(&self) -> InodeNumber {
        self.removed.number()
    }

    /// Returns the removed inode after its link-count update.
    pub const fn removed(&self) -> &Ext4Inode {
        &self.removed
    }

    /// Returns the removed directory entry file type.
    pub const fn removed_file_type(&self) -> DirectoryFileType {
        self.removed_file_type
    }
}

/// Result of linking an existing inode into one parent directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4NamespaceLink {
    parent: Ext4Inode,
    target: Ext4Inode,
}

impl Ext4NamespaceLink {
    /// Returns the updated parent directory inode.
    pub const fn parent(&self) -> &Ext4Inode {
        &self.parent
    }

    /// Returns the target inode after its link count update.
    pub const fn target(&self) -> &Ext4Inode {
        &self.target
    }
}

/// Result of renaming one namespace entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ext4NamespaceRename {
    source_parent: Ext4Inode,
    target_parent: Ext4Inode,
    moved: Ext4Inode,
    replaced: Option<Ext4Inode>,
}

impl Ext4NamespaceRename {
    /// Returns the updated source parent directory inode.
    pub const fn source_parent(&self) -> &Ext4Inode {
        &self.source_parent
    }

    /// Returns the updated target parent directory inode.
    pub const fn target_parent(&self) -> &Ext4Inode {
        &self.target_parent
    }

    /// Returns the moved inode after any directory-parent update.
    pub const fn moved(&self) -> &Ext4Inode {
        &self.moved
    }

    /// Returns the overwritten inode after its link-count update, if any.
    pub const fn replaced(&self) -> Option<&Ext4Inode> {
        self.replaced.as_ref()
    }
}

impl Ext4Filesystem {
    /// Creates a regular file in a linear ext4 directory.
    ///
    /// This is the first R7 namei write path. It keeps ext4-specific work in
    /// the storage core: inode allocation, directory-entry update, parent
    /// timestamps, JBD2 commit, and rollback on any failed intermediate step.
    /// HTree insertion, mkdir/link/unlink/rename, and zero-link eviction remain
    /// separate R7 steps.
    pub fn create_regular_file(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::create(self, parent))?;
        let transaction = handle.id();
        let result = self.create_regular_file_in_transaction(
            parent,
            name,
            permissions,
            timestamp,
            &mut handle,
        );

        match result {
            Ok(created) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(created)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Creates a subdirectory in a linear ext4 directory.
    pub fn create_directory(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::mkdir(self, parent))?;
        let transaction = handle.id();
        let result =
            self.create_directory_in_transaction(parent, name, permissions, timestamp, &mut handle);

        match result {
            Ok(created) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(created)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Creates a symbolic link in a linear ext4 directory.
    pub fn create_symlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        validate_symlink_target(target)?;
        if target.len() < disk_inode::INODE_BLOCK_BYTES {
            return self.create_fast_symlink(parent, name, target, timestamp);
        }
        self.create_block_mapped_symlink(parent, name, target, timestamp)
    }

    /// Creates a fast symbolic link in a linear ext4 directory.
    pub fn create_fast_symlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let initialization = InodeInitialization::fast_symlink(target)?
            .with_timestamp_seconds(timestamp_seconds_u32(timestamp)?);
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
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
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

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::block_mapped_symlink(self, parent))?;
        let transaction = handle.id();
        let result = self.create_block_mapped_symlink_in_transaction(
            parent,
            name,
            target,
            timestamp,
            &mut handle,
        );

        match result {
            Ok(created) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(created)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Creates a FIFO, socket, character device, or block device in a linear directory.
    pub fn create_special_file(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        kind: InodeKind,
        permissions: u16,
        device: Option<Ext4DeviceId>,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let initialization = InodeInitialization::special(kind, permissions, device)?
            .with_timestamp_seconds(timestamp_seconds_u32(timestamp)?);
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
    ) -> Ext4Result<Ext4NamespaceLink> {
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
        let file_type = directory_file_type_for_inode_kind(target.kind());

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::link(self, parent, target))?;
        let transaction = handle.id();
        let result =
            self.link_in_transaction(parent, name, target, file_type, timestamp, &mut handle);

        match result {
            Ok(linked) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(linked)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Unlinks a non-directory child from a linear ext4 directory.
    pub fn unlink(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceRemove> {
        self.ensure_namespace_mutation_supported(parent, name)?;
        let entry = self
            .lookup_bytes(parent, name)?
            .ok_or(Ext4Error::NotFound)?;
        if entry.file_type() == DirectoryFileType::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        let child = self.inode(entry.inode())?;
        if child.kind() == InodeKind::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        self.ensure_unlinked_inode_eviction_supported(&child)?;

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::unlink(self, parent, &child))?;
        let transaction = handle.id();
        let result = self.unlink_in_transaction(parent, name, &child, timestamp, &mut handle);

        match result {
            Ok(removed) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(removed)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Removes an empty subdirectory from a linear ext4 directory.
    pub fn remove_directory(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceRemove> {
        self.ensure_namespace_mutation_supported(parent, name)?;
        let entry = self
            .lookup_bytes(parent, name)?
            .ok_or(Ext4Error::NotFound)?;
        if entry.file_type() != DirectoryFileType::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        let child = self.inode(entry.inode())?;
        if child.kind() != InodeKind::Directory {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.ensure_empty_linear_directory(parent, &child)?;
        self.ensure_unlinked_inode_eviction_supported(&child)?;

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::rmdir(self, parent, &child))?;
        let transaction = handle.id();
        let result =
            self.remove_directory_in_transaction(parent, name, &child, timestamp, &mut handle);

        match result {
            Ok(removed) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(removed)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    /// Renames a child between supported linear ext4 directories.
    pub fn rename(
        &mut self,
        source_parent: &Ext4Inode,
        source_name: &[u8],
        target_parent: &Ext4Inode,
        target_name: &[u8],
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceRename> {
        self.ensure_namespace_mutation_supported(source_parent, source_name)?;
        self.ensure_namespace_mutation_supported(target_parent, target_name)?;
        let same_parent = source_parent.number() == target_parent.number();
        if same_parent && source_name == target_name {
            let moved = self
                .lookup_bytes(source_parent, source_name)?
                .ok_or(Ext4Error::NotFound)
                .and_then(|entry| self.inode(entry.inode()))?;
            return Ok(Ext4NamespaceRename {
                source_parent: source_parent.clone(),
                target_parent: target_parent.clone(),
                moved,
                replaced: None,
            });
        }

        let source_entry = self
            .lookup_bytes(source_parent, source_name)?
            .ok_or(Ext4Error::NotFound)?;
        let moved = self.inode(source_entry.inode())?;
        let target_entry = self.lookup_bytes(target_parent, target_name)?;
        let target_inode = target_entry
            .as_ref()
            .map(|entry| self.inode(entry.inode()))
            .transpose()?;

        if target_inode
            .as_ref()
            .is_some_and(|target| target.number() == moved.number())
        {
            return Ok(Ext4NamespaceRename {
                source_parent: source_parent.clone(),
                target_parent: target_parent.clone(),
                moved,
                replaced: None,
            });
        }

        self.ensure_rename_type_supported(&moved, target_inode.as_ref())?;
        if let Some(target) = target_inode.as_ref() {
            if target.kind() == InodeKind::Directory {
                self.ensure_empty_linear_directory(target_parent, target)?;
                self.ensure_unlinked_inode_eviction_supported(target)?;
            } else if target.links_count() == 1 {
                self.ensure_unlinked_inode_eviction_supported(target)?;
            }
        }
        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::rename(
            self,
            source_parent,
            target_parent,
            &moved,
            target_inode.as_ref(),
        ))?;
        let transaction = handle.id();
        let result = self.rename_in_transaction(
            source_parent,
            source_name,
            target_parent,
            target_name,
            &moved,
            target_inode.as_ref(),
            timestamp,
            &mut handle,
        );

        match result {
            Ok(renamed) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(renamed)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    fn create_regular_file_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let timestamp_seconds = timestamp_seconds_u32(timestamp)?;
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::regular_file(permissions)
                .with_timestamp_seconds(timestamp_seconds),
            handle,
        )?;
        let child = self.internal_inode(allocation.inode())?;
        let parent = self.insert_directory_entry(
            parent,
            name,
            child.number(),
            DirectoryFileType::RegularFile,
            timestamp,
            handle,
        )?;

        Ok(Ext4NamespaceCreate { parent, child })
    }

    fn create_directory_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        permissions: u16,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let timestamp_seconds = timestamp_seconds_u32(timestamp)?;
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::directory(permissions).with_timestamp_seconds(timestamp_seconds),
            handle,
        )?;
        let mut child = self.internal_inode(allocation.inode())?;
        child = self.initialize_directory_data_block(&child, parent.number(), timestamp, handle)?;
        let mut parent = self.insert_directory_entry(
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
        parent =
            self.update_inode_links_count_metadata(&parent, parent_links, timestamp, handle)?;

        Ok(Ext4NamespaceCreate { parent, child })
    }

    fn create_initialized_child(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        initialization: InodeInitialization,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        self.ensure_namespace_create_supported(parent, name)?;
        if self.lookup_bytes(parent, name)?.is_some() {
            return Err(Ext4Error::AlreadyExists);
        }

        let journal = self.namei_metadata_journal()?;
        let mut handle = journal.begin(Ext4Credits::create(self, parent))?;
        let transaction = handle.id();
        let result = self.create_initialized_child_in_transaction(
            parent,
            name,
            initialization,
            file_type,
            timestamp,
            &mut handle,
        );

        match result {
            Ok(created) => {
                drop(handle);
                self.commit_namei_transaction(journal, transaction)?;
                Ok(created)
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    fn create_initialized_child_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        initialization: InodeInitialization,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let allocation =
            self.allocate_named_inode(Some(parent.number()), name, initialization, handle)?;
        let child = self.internal_inode(allocation.inode())?;
        let parent = self.insert_directory_entry(
            parent,
            name,
            child.number(),
            file_type,
            timestamp,
            handle,
        )?;
        Ok(Ext4NamespaceCreate { parent, child })
    }

    fn create_block_mapped_symlink_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &[u8],
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceCreate> {
        let timestamp_seconds = timestamp_seconds_u32(timestamp)?;
        let allocation = self.allocate_named_inode(
            Some(parent.number()),
            name,
            InodeInitialization::block_mapped_symlink(target.len())?
                .with_timestamp_seconds(timestamp_seconds),
            handle,
        )?;
        let child = self.internal_inode(allocation.inode())?;
        let child = self.initialize_symlink_data_block(&child, target, handle)?;
        let parent = self.insert_directory_entry(
            parent,
            name,
            child.number(),
            DirectoryFileType::Symlink,
            timestamp,
            handle,
        )?;
        Ok(Ext4NamespaceCreate { parent, child })
    }

    fn link_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        target: &Ext4Inode,
        file_type: DirectoryFileType,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceLink> {
        let links_count = target
            .links_count()
            .checked_add(1)
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::LinkCountLimit))?;
        let target =
            self.update_inode_links_count_ctime_metadata(target, links_count, timestamp, handle)?;
        let parent = self.insert_directory_entry(
            parent,
            name,
            target.number(),
            file_type,
            timestamp,
            handle,
        )?;
        Ok(Ext4NamespaceLink { parent, target })
    }

    fn unlink_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceRemove> {
        let removed = self.remove_directory_entry(parent, name, timestamp, handle)?;
        if removed.inode != child.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let removed_inode = self.finish_removed_inode(child, None, timestamp, handle)?;

        Ok(Ext4NamespaceRemove {
            parent: removed.parent,
            removed: removed_inode,
            removed_file_type: removed.file_type,
        })
    }

    fn remove_directory_in_transaction(
        &mut self,
        parent: &Ext4Inode,
        name: &[u8],
        child: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4NamespaceRemove> {
        let removed = self.remove_directory_entry(parent, name, timestamp, handle)?;
        if removed.inode != child.number() || removed.file_type != DirectoryFileType::Directory {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let parent_links = removed
            .parent
            .links_count()
            .checked_sub(1)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
        let parent = self.update_inode_links_count_metadata(
            &removed.parent,
            parent_links,
            timestamp,
            handle,
        )?;
        let removed_inode = self.finish_removed_inode(child, Some(0), timestamp, handle)?;

        Ok(Ext4NamespaceRemove {
            parent,
            removed: removed_inode,
            removed_file_type: removed.file_type,
        })
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
    ) -> Ext4Result<Ext4NamespaceRename> {
        let same_parent = source_parent.number() == target_parent.number();
        let file_type = directory_file_type_for_inode_kind(moved.kind());
        let (source, target) = self.rename_prepare(
            source_parent,
            source_name,
            target_parent,
            target_name,
            moved,
            replaced,
            file_type,
        );

        let (mut target_parent_current, replaced_entry) =
            self.setent_or_add_entry(&source, &target, timestamp, handle)?;
        let mut source_parent_current = if same_parent {
            target_parent_current.clone()
        } else {
            source_parent.clone()
        };
        let moved = self.update_inode_ctime_metadata(source.inode, timestamp, handle)?;

        let removed_source =
            self.rename_delete_old(&source, &source_parent_current, timestamp, handle)?;
        source_parent_current = removed_source.parent;
        if same_parent {
            target_parent_current = source_parent_current.clone();
        }

        let (source_parent_current, target_parent_current, moved) = self.rename_dir_finish(
            source_parent_current,
            target_parent_current,
            &moved,
            target.parent,
            target.replaced,
            same_parent,
            timestamp,
            handle,
        )?;
        let replaced = target
            .replaced
            .map(|replaced| self.finish_replaced_inode(replaced, replaced_entry, timestamp, handle))
            .transpose()?;

        Ok(Ext4NamespaceRename {
            source_parent: source_parent_current,
            target_parent: target_parent_current,
            moved,
            replaced,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_prepare<'a>(
        &self,
        source_parent: &'a Ext4Inode,
        source_name: &'a [u8],
        target_parent: &'a Ext4Inode,
        target_name: &'a [u8],
        moved: &'a Ext4Inode,
        replaced: Option<&'a Ext4Inode>,
        file_type: DirectoryFileType,
    ) -> (RenameEntry<'a>, RenameTarget<'a>) {
        (
            RenameEntry {
                parent: source_parent,
                name: source_name,
                inode: moved,
                file_type,
            },
            RenameTarget {
                parent: target_parent,
                name: target_name,
                replaced,
            },
        )
    }

    fn setent_or_add_entry(
        &mut self,
        source: &RenameEntry<'_>,
        target: &RenameTarget<'_>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<(Ext4Inode, Option<ReplacedDirectoryEntry>)> {
        let Some(replaced) = target.replaced else {
            let parent = self.insert_directory_entry(
                target.parent,
                target.name,
                source.inode.number(),
                source.file_type,
                timestamp,
                handle,
            )?;
            return Ok((parent, None));
        };

        let replaced_entry = self.replace_directory_entry(
            target.parent,
            target.name,
            source.inode.number(),
            source.file_type,
            timestamp,
            handle,
        )?;
        if replaced_entry.inode != replaced.number()
            || replaced_entry.file_type != directory_file_type_for_inode_kind(replaced.kind())
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok((replaced_entry.parent.clone(), Some(replaced_entry)))
    }

    fn rename_delete_old(
        &mut self,
        source: &RenameEntry<'_>,
        current_parent: &Ext4Inode,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<RemovedLinearDirectoryEntry> {
        if current_parent.number() != source.parent.number() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let removed =
            self.remove_directory_entry(current_parent, source.name, timestamp, handle)?;
        if removed.inode != source.inode.number() || removed.file_type != source.file_type {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(removed)
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_dir_finish(
        &mut self,
        mut source_parent: Ext4Inode,
        mut target_parent: Ext4Inode,
        moved: &Ext4Inode,
        target_parent_original: &Ext4Inode,
        replaced: Option<&Ext4Inode>,
        same_parent: bool,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<(Ext4Inode, Ext4Inode, Ext4Inode)> {
        let mut moved = moved.clone();
        if moved.kind() != InodeKind::Directory {
            return Ok((source_parent, target_parent, moved));
        }

        let replaced_directory = replaced.is_some_and(|inode| inode.kind() == InodeKind::Directory);
        if !same_parent || replaced_directory {
            let source_links = source_parent
                .links_count()
                .checked_sub(1)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?;
            source_parent = self.update_inode_links_count_metadata(
                &source_parent,
                source_links,
                timestamp,
                handle,
            )?;
            if same_parent {
                target_parent = source_parent.clone();
            }
        }

        if !same_parent && !replaced_directory {
            let target_links = target_parent
                .links_count()
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?;
            target_parent = self.update_inode_links_count_metadata(
                &target_parent,
                target_links,
                timestamp,
                handle,
            )?;
        }

        if !same_parent {
            moved = self.update_directory_dotdot_entry(
                &moved,
                target_parent_original.number(),
                timestamp,
                handle,
            )?;
        }

        Ok((source_parent, target_parent, moved))
    }

    fn finish_replaced_inode(
        &mut self,
        inode: &Ext4Inode,
        replaced_entry: Option<ReplacedDirectoryEntry>,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
    ) -> Ext4Result<Ext4Inode> {
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
        let orphaned = self.add_namespace_orphan(inode, handle)?;
        self.update_unlinked_inode_metadata(&orphaned, zero_link_size, timestamp, handle)
    }

    /// Releases a zero-link inode after its final VFS reference is gone.
    pub fn evict_unlinked_inode(
        &mut self,
        number: InodeNumber,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        let inode = self.orphan_inode(number)?;
        if inode.links_count() != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        self.evict_unlinked_inode_with_policy(
            &inode,
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

        let journal = self.namei_metadata_journal()?;
        let mut handle =
            journal.begin(Ext4Credits::credits(Ext4Credits::zero_link_eviction(inode)))?;
        let transaction = handle.id();
        let result = self.evict_zero_link_inode(inode, None, timestamp, &mut handle);
        match result {
            Ok(()) => {
                drop(handle);
                self.commit_namei_transaction_with_policy(
                    journal,
                    transaction,
                    recovery_flag_policy,
                )
            }
            Err(error) => Err(self.abort_namei_transaction(&journal, error)),
        }
    }

    fn initialize_directory_data_block(
        &mut self,
        directory: &Ext4Inode,
        parent: InodeNumber,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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

        let inode = self.insert_extent_mapping(
            directory,
            LogicalBlock::new(0),
            allocation.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        self.update_inode_size_metadata(&inode, block_size_u64, timestamp, handle)
    }

    fn initialize_symlink_data_block(
        &mut self,
        symlink: &Ext4Inode,
        target: &[u8],
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
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
        self.write_contiguous_blocks(block, 1, &bytes)?;

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
        let orphaned = self.add_namespace_orphan(inode, handle)?;
        let without_xattr = self.release_external_xattr_block_for_eviction(&orphaned, handle)?;
        let truncated = if self.unlinked_inode_data_blocks(&without_xattr)? == 0 {
            without_xattr
        } else {
            self.truncate_extent_mappings(&without_xattr, LogicalBlock::new(0), handle)?
        };
        let unlinked = self.update_unlinked_inode_metadata(&truncated, size, timestamp, handle)?;
        let unlinked = self.remove_orphan(&unlinked, handle)?;
        self.release_allocated_inode(unlinked.number(), unlinked.kind(), handle)?;
        Ok(())
    }

    fn update_directory_dotdot_entry(
        &mut self,
        directory: &Ext4Inode,
        parent: InodeNumber,
        timestamp: Ext4Timestamp,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut block = vec![0; block_size];
        let physical = match self.map_blocks(directory, LogicalBlock::new(0))? {
            BlockMapping::Mapped { physical, len } if physical.get() != 0 && len.get() != 0 => {
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
        block[..block_size].copy_from_slice(&buffer.as_ref()[..block_size]);
        if directory.has_indexed_directory() {
            let count_limit = self.decode_htree_root_for_write(directory, &block, block_size)?;
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
            .undo_access(FilesystemBlock::new(physical.get()), handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        let root_count_limit = if directory.has_indexed_directory() {
            let count_limit = self.decode_htree_root_for_write(directory, &bytes, block_size)?;
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
    ) -> Ext4Result<Ext4Inode> {
        if directory.has_indexed_directory() {
            return self.insert_indexed_directory_entry(
                directory, name, inode, file_type, timestamp, handle,
            );
        }
        self.insert_linear_directory_entry(directory, name, inode, file_type, timestamp, handle)
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
        let access = self.metadata_io.undo_access(filesystem_block, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        self.verify_directory_block(directory, logical, &bytes)?;
        let slot =
            find_linear_entry_slot(&bytes, block_size, name, self.superblock().inodes_count())?
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        let replaced = slot.replace(&mut bytes, inode, file_type)?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        let parent = self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
        Ok(ReplacedDirectoryEntry {
            parent,
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
    ) -> Ext4Result<Ext4Inode> {
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
                BlockMapping::Mapped { physical, len } if physical.get() != 0 && len.get() != 0 => {
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
                let access = self.metadata_io.undo_access(filesystem_block, handle)?;
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
    ) -> Ext4Result<Ext4Inode> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let root_physical = self.mapped_directory_block(directory, 0)?;
        let mut old_root = vec![0; block_size];
        let root_buffer = self.read_metadata_block(FilesystemBlock::new(root_physical.get()))?;
        old_root[..block_size].copy_from_slice(&root_buffer.as_ref()[..block_size]);
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
            .undo_access(FilesystemBlock::new(root_physical.get()), handle)?;
        replace_metadata_access_bytes(&root_access, root)?;

        let leaf_access = self
            .metadata_io
            .create_access(FilesystemBlock::new(new_physical.get()), handle)?;
        replace_metadata_access_bytes(&leaf_access, leaf)?;

        let directory = self.insert_extent_mapping(
            directory,
            LogicalBlock::new(new_logical),
            new_physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        let directory = self.update_inode_size_metadata(
            &directory,
            directory
                .size()
                .checked_add(block_size_u64)
                .ok_or(Ext4Error::Overflow)?,
            timestamp,
            handle,
        )?;
        let directory = self.update_inode_flags_timestamps_metadata(
            &directory,
            directory.flags() | disk_inode::EXT4_INDEX_FL,
            timestamp,
            handle,
        )?;
        self.insert_indexed_directory_entry(&directory, name, inode, file_type, timestamp, handle)
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
                BlockMapping::Mapped { physical, len } if physical.get() != 0 && len.get() != 0 => {
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
            let access = self.metadata_io.undo_access(filesystem_block, handle)?;
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
            let parent = self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
            return Ok(RemovedLinearDirectoryEntry {
                parent,
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
    ) -> Ext4Result<Ext4Inode> {
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
        let access = self.metadata_io.undo_access(filesystem_block, handle)?;
        let mut bytes = metadata_access_bytes(&access)?;
        self.verify_directory_block(directory, logical, &bytes)?;
        let slot =
            find_linear_remove_slot(&bytes, block_size, name, self.superblock().inodes_count())?
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))?;
        let removed = slot.remove(&mut bytes, block_size)?;
        self.update_directory_block_checksum(directory, &mut bytes)?;
        replace_metadata_access_bytes(&access, bytes)?;
        let parent = self.update_inode_timestamps_metadata(directory, timestamp, handle)?;
        Ok(RemovedLinearDirectoryEntry {
            parent,
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
        bytes[..block_size].copy_from_slice(&buffer.as_ref()[..block_size]);
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
        root[..block_size].copy_from_slice(&root_buffer.as_ref()[..block_size]);
        let root_info = disk_dir::HTreeRootInfo::decode(&root)?;
        let root_count_limit = self.decode_htree_root_for_write(directory, &root, block_size)?;
        self.verify_htree_block_checksum(
            directory,
            0,
            &root,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            root_count_limit,
        )?;
        let hash = directory_hash(
            name,
            root_info.hash_version(),
            self.superblock().hash_seed(),
        )?;
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
        node[..block_size].copy_from_slice(&node_buffer.as_ref()[..block_size]);
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
            .undo_access(FilesystemBlock::new(physical.get()), handle)?;
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
    ) -> Ext4Result<Ext4Inode> {
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
        let mut records = collect_leaf_records_for_split(
            &old_bytes,
            block_size,
            target.hash_version,
            self.superblock().hash_seed(),
            self.superblock().inodes_count(),
        )?;
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
        let allocation = self.allocate_block(None, handle)?;
        let new_physical = allocation.block();

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

        let old_access = self
            .metadata_io
            .undo_access(FilesystemBlock::new(target.leaf_physical.get()), handle)?;
        replace_metadata_access_bytes(&old_access, left_bytes)?;

        let new_access = self
            .metadata_io
            .create_access(FilesystemBlock::new(new_physical.get()), handle)?;
        replace_metadata_access_bytes(&new_access, right_bytes)?;

        let directory = self.insert_extent_mapping(
            directory,
            LogicalBlock::new(new_logical),
            new_physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        let directory = self.update_inode_size_metadata(
            &directory,
            directory
                .size()
                .checked_add(block_size_u64)
                .ok_or(Ext4Error::Overflow)?,
            timestamp,
            handle,
        )?;

        let parent_access = self
            .metadata_io
            .undo_access(FilesystemBlock::new(target.parent.physical.get()), handle)?;
        let mut parent_bytes = metadata_access_bytes(&parent_access)?;
        self.verify_htree_block_checksum(
            &directory,
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
            &directory,
            &mut parent_bytes,
            target.parent.count_offset,
            count_limit,
        )?;
        replace_metadata_access_bytes(&parent_access, parent_bytes)?;
        self.update_inode_timestamps_metadata(&directory, timestamp, handle)
    }

    fn decode_htree_root_for_write(
        &self,
        _directory: &Ext4Inode,
        bytes: &[u8],
        block_size: usize,
    ) -> Ext4Result<disk_dir::HTreeCountLimit> {
        let root_info = disk_dir::HTreeRootInfo::decode(bytes)?;
        if root_info.reserved_zero() != 0
            || root_info.info_length() != 8
            || root_info.flags() != 0
            || root_info.hash_version() > crate::dirhash::DX_HASH_TEA_UNSIGNED
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if root_info.indirect_levels() > 1 {
            return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
        }
        self.decode_htree_count_limit(bytes, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET, block_size)
    }

    fn decode_htree_node_for_write(
        &self,
        directory: &Ext4Inode,
        logical_block: u64,
        bytes: &[u8],
        block_size: usize,
    ) -> Ext4Result<disk_dir::HTreeCountLimit> {
        let fake = disk_dir::RawDirectoryEntry::decode(bytes, 0)?;
        let fake_len = crate::dir::rec_len_from_disk(fake.rec_len(), block_size)?;
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
        let tail_reserved = bytes
            .get(tail_offset..tail_offset + 4)
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
        put_u32(bytes, tail_offset + 4, checksum)
    }

    fn mapped_directory_block(
        &self,
        directory: &Ext4Inode,
        logical: u64,
    ) -> Ext4Result<PhysicalBlock> {
        match self.map_blocks(directory, LogicalBlock::new(logical))? {
            BlockMapping::Mapped { physical, len } if physical.get() != 0 && len.get() != 0 => {
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
        let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
        output[..block_size].copy_from_slice(&buffer.as_ref()[..block_size]);
        self.verify_directory_block(directory, logical, &output[..block_size])?;
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
    ) -> Ext4Result<Ext4Inode> {
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
        let inode = self.insert_extent_mapping(
            directory,
            LogicalBlock::new(logical),
            allocation.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            handle,
        )?;
        self.update_inode_size_metadata(
            &inode,
            directory
                .size()
                .checked_add(block_size_u64)
                .ok_or(Ext4Error::Overflow)?,
            timestamp,
            handle,
        )
    }

    fn namei_metadata_journal(&mut self) -> Ext4Result<Journal> {
        self.metadata_journal()
    }

    fn commit_namei_transaction(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.commit_metadata_transaction(journal, transaction)
    }

    fn commit_namei_transaction_with_policy(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
        recovery_flag_policy: crate::journal::RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.commit_metadata_transaction_with_policy(journal, transaction, recovery_flag_policy)
    }

    fn abort_namei_transaction(&mut self, journal: &Journal, error: Ext4Error) -> Ext4Error {
        if let Some(undo) = journal.abort(error)
            && let Err(rollback_error) = self.rollback_metadata_undo(&undo)
        {
            return rollback_error;
        }
        error
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RemovedLinearDirectoryEntry {
    parent: Ext4Inode,
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReplacedDirectoryEntry {
    parent: Ext4Inode,
    inode: InodeNumber,
    file_type: DirectoryFileType,
}

#[derive(Clone, Copy, Debug)]
struct RenameEntry<'a> {
    parent: &'a Ext4Inode,
    name: &'a [u8],
    inode: &'a Ext4Inode,
    file_type: DirectoryFileType,
}

#[derive(Clone, Copy, Debug)]
struct RenameTarget<'a> {
    parent: &'a Ext4Inode,
    name: &'a [u8],
    replaced: Option<&'a Ext4Inode>,
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
                    rec_len_to_disk(existing_rec_len, block_size)?,
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
                    rec_len_to_disk(merged_len, block_size)?,
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
        let rec_len = crate::dir::rec_len_from_disk(entry.rec_len(), block_size)?;
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
        let rec_len = crate::dir::rec_len_from_disk(entry.rec_len(), block_size)?;
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
        let rec_len = crate::dir::rec_len_from_disk(entry.rec_len(), block_size)?;
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

fn collect_leaf_records_for_split(
    bytes: &[u8],
    block_size: usize,
    hash_version: u8,
    hash_seed: [u32; 4],
    inodes_count: u32,
) -> Ext4Result<Vec<DirectoryLeafRecord>> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = crate::dir::rec_len_from_disk(entry.rec_len(), block_size)?;
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
            records.push(DirectoryLeafRecord {
                hash: directory_hash(name, hash_version, hash_seed)?.major(),
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
        let rec_len = crate::dir::rec_len_from_disk(entry.rec_len(), block_size)?;
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

fn timestamp_seconds_u32(timestamp: Ext4Timestamp) -> Ext4Result<u32> {
    u32::try_from(timestamp.seconds())
        .map_err(|_| Ext4Error::Unsupported(UnsupportedKind::TimestampRange))
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

fn rec_len_to_disk(len: usize, block_size: usize) -> Ext4Result<u16> {
    if len == 0 || !len.is_multiple_of(4) || len > block_size {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
    }
    if len == block_size && block_size > u16::MAX as usize {
        return Ok(u16::MAX);
    }
    if block_size > u16::MAX as usize {
        let encoded = (len & 0xfffc) | ((len >> 16) & 0x3);
        return u16::try_from(encoded).map_err(|_| Ext4Error::Overflow);
    }
    u16::try_from(len).map_err(|_| Ext4Error::Overflow)
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
    put_u16(record, 4, rec_len_to_disk(rec_len, block_size)?)?;
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
    put_u16(record, 4, rec_len_to_disk(rec_len, block_size)?)
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
