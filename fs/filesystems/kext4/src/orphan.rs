// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy ext4 regular-file orphan-list helpers.

use crate::{
    CorruptKind, Ext4Error, Ext4Filesystem, Ext4Inode, Ext4Result, InodeNumber, UnsupportedKind,
    disk::superblock,
    journal::RecoveryFlagPolicy,
    superblock::{metadata_access_bytes, replace_metadata_access_bytes},
};

impl Ext4Filesystem {
    pub(crate) fn orphan_head(&self) -> Option<InodeNumber> {
        match self.superblock().last_orphan() {
            0 => None,
            head => Some(InodeNumber::new(head)),
        }
    }

    pub(crate) fn add_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_legacy_orphan_list_supported()?;
        self.ensure_regular_file_orphan_inode_supported(inode)?;
        self.add_orphan_to_legacy_list(inode, handle)
    }

    pub(crate) fn add_namespace_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.ensure_legacy_orphan_list_supported()?;
        self.validate_orphan_number(inode.number())?;
        self.add_orphan_to_legacy_list(inode, handle)
    }

    fn add_orphan_to_legacy_list(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        if self.orphan_list_contains(inode.number())? {
            return Ok(inode.clone());
        }

        let head = self.orphan_head();
        let updated_inode = self.update_inode_orphan_next(inode, head, handle)?;
        self.set_orphan_head(Some(inode.number()), handle)?;
        Ok(updated_inode)
    }

    pub(crate) fn remove_orphan(
        &mut self,
        inode: &Ext4Inode,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        self.remove_orphan_inner(inode, true, handle)
    }

    pub(crate) fn cleanup_legacy_orphans(&mut self) -> Ext4Result<usize> {
        self.cleanup_legacy_orphans_with_policy(RecoveryFlagPolicy::ClearAfterCheckpoint)
    }

    pub(crate) fn cleanup_legacy_orphans_preserving_recovery(&mut self) -> Ext4Result<usize> {
        self.cleanup_legacy_orphans_with_policy(RecoveryFlagPolicy::PreserveDuringRecovery)
    }

    fn cleanup_legacy_orphans_with_policy(
        &mut self,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<usize> {
        self.ensure_legacy_orphan_list_supported()?;
        if self.orphan_head().is_some() && self.journal.is_none() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite));
        }
        let mut cleaned = 0usize;
        let mut steps = 0u32;
        while let Some(head) = self.orphan_head() {
            self.advance_orphan_walk(&mut steps)?;
            self.validate_orphan_number(head)?;
            let inode = self.orphan_inode(head)?;
            if inode.links_count() == 0 {
                self.cleanup_unlinked_orphan_from_head(&inode, recovery_flag_policy)?;
            } else {
                self.cleanup_regular_file_orphan_from_head(&inode, recovery_flag_policy)?;
            }
            cleaned = cleaned.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
        Ok(cleaned)
    }

    fn remove_orphan_inner(
        &mut self,
        inode: &Ext4Inode,
        clear_target_dtime: bool,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<Ext4Inode> {
        let mut previous = None;
        let mut current = self.orphan_head();
        let mut steps = 0u32;
        while let Some(number) = current {
            self.advance_orphan_walk(&mut steps)?;
            self.validate_orphan_number(number)?;

            let current_inode = self.orphan_inode(number)?;
            let next = self.valid_orphan_next(current_inode.orphan_next())?;
            if number == inode.number() {
                match previous {
                    Some(previous) => {
                        let previous_inode = self.orphan_inode(previous)?;
                        let _ = self.update_inode_orphan_next(&previous_inode, next, handle)?;
                    }
                    None => self.set_orphan_head(next, handle)?,
                }
                if clear_target_dtime {
                    return self.update_inode_orphan_next(&current_inode, None, handle);
                }
                return Ok(current_inode);
            }

            previous = Some(number);
            current = next;
        }

        Ok(inode.clone())
    }

    fn orphan_list_contains(&self, inode: InodeNumber) -> Ext4Result<bool> {
        let mut current = self.orphan_head();
        let mut steps = 0u32;
        while let Some(number) = current {
            self.advance_orphan_walk(&mut steps)?;
            self.validate_orphan_number(number)?;
            if number == inode {
                return Ok(true);
            }
            current = self.valid_orphan_next(self.orphan_inode(number)?.orphan_next())?;
        }
        Ok(false)
    }

    pub(crate) fn set_orphan_head(
        &mut self,
        head: Option<InodeNumber>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let head = head.map_or(0, |inode| inode.get());
        if head != 0 {
            self.validate_orphan_number(InodeNumber::new(head))?;
        }

        let (block, offset, len) = self.primary_superblock_location()?;
        let access = self.metadata_io.undo_access(block, handle)?;
        let mut block_bytes = metadata_access_bytes(&access)?;
        let superblock_bytes = block_bytes
            .get_mut(offset..offset + len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let updated = superblock::set_last_orphan(superblock_bytes, head)?;
        replace_metadata_access_bytes(&access, block_bytes)?;
        self.superblock = updated;
        Ok(())
    }

    pub(crate) fn ensure_legacy_orphan_list_supported(&self) -> Ext4Result<()> {
        let features = self.superblock().features();
        if features.has_orphan_file() || features.has_orphan_present() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile));
        }
        Ok(())
    }

    fn ensure_regular_file_orphan_inode_supported(&self, inode: &Ext4Inode) -> Ext4Result<()> {
        self.validate_orphan_number(inode.number())?;
        self.ensure_regular_file_mutation_supported(inode)
    }

    pub(crate) fn validate_orphan_number(&self, inode: InodeNumber) -> Ext4Result<()> {
        if inode.get() == 0
            || inode.get() > self.superblock().inodes_count()
            || self.is_reserved_inode(inode)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
        }
        Ok(())
    }

    pub(crate) fn valid_orphan_next(
        &self,
        next: Option<InodeNumber>,
    ) -> Ext4Result<Option<InodeNumber>> {
        if let Some(next) = next {
            self.validate_orphan_number(next)?;
        }
        Ok(next)
    }

    fn advance_orphan_walk(&self, steps: &mut u32) -> Ext4Result<()> {
        *steps = steps.checked_add(1).ok_or(Ext4Error::Overflow)?;
        if *steps > self.superblock().inodes_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
        }
        Ok(())
    }
}
