// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::num::NonZeroU32;

use block::BlockDevice;

use crate::{
    bitmap_allocator::{BlockGroupRange, InodeGroupRange},
    buffer::{Ext4MetadataIo, MetadataBuffer, MetadataWriteAccess},
    dirhash::directory_hash,
    disk::{BlockGroupDescriptor, Superblock, checksum, features, superblock},
    error::{ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, UnsupportedKind},
    extent::BlockMapping,
    inode::InodeKind,
    io::FilesystemDevice,
    jbd2::{
        JournalBlock, JournalBlockMapper, JournalLogScan, JournalReplayApplied,
        JournalReplayReport, JournalStart, JournalSuperblock, replay_scanned_journal, scan_journal,
    },
    journal::MountedJournal,
    mballoc::BlockGroupFreeExtentCache,
    types::{BlockGroupNumber, FilesystemBlock, InodeNumber, LogicalBlock},
};

/// Immutable geometry derived from a validated ext4 superblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemLayout {
    block_size: u32,
    block_count: u64,
    group_count: u32,
    descriptor_size: u16,
    descriptor_table_start: FilesystemBlock,
    descriptor_table_blocks: u32,
    inode_table_blocks_per_group: u32,
}

/// ext4 filesystem statistics computed by the storage core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4StatFs {
    /// Fundamental block size in bytes.
    pub block_size: u32,
    /// Fragment size in bytes.
    pub fragment_size: u32,
    /// Total data blocks visible through statfs.
    pub blocks: u64,
    /// Free blocks in the filesystem.
    pub blocks_free: u64,
    /// Free blocks available after privileged reservations.
    pub blocks_available: u64,
    /// Total inode count.
    pub files: u64,
    /// Free inode count.
    pub files_free: u64,
    /// Maximum ext4 filename length in bytes.
    pub max_name_len: u32,
}

#[derive(Clone, Copy, Debug)]
struct InodeAllocationTotals {
    free_inodes: u64,
    free_blocks: u64,
    used_directories: u64,
}

#[derive(Clone, Copy, Debug)]
struct FlexGroupStats {
    free_inodes: u64,
    free_blocks: u64,
    used_directories: u64,
}

/// Public summary of the internal journal superblock state.
///
/// This deliberately exposes only ext4 mount/recovery status. JBD2 runtime
/// types such as transaction handles and journal blocks remain crate-internal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalStatus {
    block_size: u32,
    sequence: u32,
    start_block: Option<u32>,
    head: u32,
}

impl JournalStatus {
    /// Returns the journal block size in bytes.
    pub const fn block_size(self) -> u32 {
        self.block_size
    }

    /// Returns the transaction sequence recorded in the journal superblock.
    pub const fn sequence(self) -> u32 {
        self.sequence
    }

    /// Returns the raw nonzero journal start block when the journal is active.
    pub const fn start_block(self) -> Option<u32> {
        self.start_block
    }

    /// Returns whether the journal superblock records an active log start.
    pub const fn has_nonzero_log_start(self) -> bool {
        self.start_block.is_some()
    }

    /// Returns the recorded journal head block.
    pub const fn head(self) -> u32 {
        self.head
    }
}

impl From<&JournalSuperblock> for JournalStatus {
    fn from(superblock: &JournalSuperblock) -> Self {
        let start_block = match superblock.start() {
            JournalStart::Zero => None,
            JournalStart::Block(block) => Some(block.get()),
        };
        Self {
            block_size: superblock.block_size(),
            sequence: superblock.sequence().get(),
            start_block,
            head: superblock.head().get(),
        }
    }
}

/// Public summary of explicit ext4 journal recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4RecoveryReport {
    update_count: usize,
    revoke_hit_count: usize,
    head: u32,
    next_sequence: u32,
}

impl Ext4RecoveryReport {
    pub(crate) fn from_journal_report(report: JournalReplayReport) -> Self {
        Self {
            update_count: report.update_count(),
            revoke_hit_count: report.revoke_hit_count(),
            head: report.head().get(),
            next_sequence: report.next_sequence().get(),
        }
    }

    /// Returns how many metadata blocks were written by replay.
    pub const fn update_count(self) -> usize {
        self.update_count
    }

    /// Returns how many descriptor updates were suppressed by revoke records.
    pub const fn revoke_hit_count(self) -> usize {
        self.revoke_hit_count
    }

    /// Returns the first journal block after the recovered log contents.
    pub const fn head(self) -> u32 {
        self.head
    }

    /// Returns the sequence expected for the next transaction.
    pub const fn next_sequence(self) -> u32 {
        self.next_sequence
    }
}

impl FilesystemLayout {
    fn derive(superblock: &Superblock) -> Ext4Result<Self> {
        let data_blocks = superblock
            .blocks_count()
            .checked_sub(u64::from(superblock.first_data_block()))
            .ok_or(Ext4Error::Corrupt(CorruptKind::ZeroGeometry))?;
        let blocks_per_group = u64::from(superblock.blocks_per_group());
        let group_count = data_blocks
            .checked_add(blocks_per_group - 1)
            .ok_or(Ext4Error::Overflow)?
            / blocks_per_group;
        let group_count = u32::try_from(group_count).map_err(|_| Ext4Error::Overflow)?;
        let descriptor_size = superblock.descriptor_size();
        let descriptor_bytes = u64::from(group_count)
            .checked_mul(u64::from(descriptor_size))
            .ok_or(Ext4Error::Overflow)?;

        let inode_table_bytes = u64::from(superblock.inodes_per_group())
            .checked_mul(u64::from(superblock.inode_size()))
            .ok_or(Ext4Error::Overflow)?;
        let block_size = u64::from(superblock.block_size());
        let descriptor_table_blocks = descriptor_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let descriptor_table_blocks =
            u32::try_from(descriptor_table_blocks).map_err(|_| Ext4Error::Overflow)?;
        let inode_table_blocks_per_group = inode_table_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let inode_table_blocks_per_group =
            u32::try_from(inode_table_blocks_per_group).map_err(|_| Ext4Error::Overflow)?;

        Ok(Self {
            block_size: superblock.block_size(),
            block_count: superblock.blocks_count(),
            group_count,
            descriptor_size,
            descriptor_table_start: FilesystemBlock::new(if superblock.block_size() == 1024 {
                2
            } else {
                1
            }),
            descriptor_table_blocks,
            inode_table_blocks_per_group,
        })
    }

    /// Returns the filesystem block size in bytes.
    pub const fn block_size(self) -> u32 {
        self.block_size
    }

    /// Returns the total filesystem block count.
    pub const fn block_count(self) -> u64 {
        self.block_count
    }

    /// Returns the number of block groups.
    pub const fn group_count(self) -> u32 {
        self.group_count
    }

    /// Returns the size of one block group descriptor.
    pub const fn descriptor_size(self) -> u16 {
        self.descriptor_size
    }

    /// Returns the first block of the primary group descriptor table.
    pub const fn descriptor_table_start(self) -> FilesystemBlock {
        self.descriptor_table_start
    }

    /// Returns the number of blocks occupied by the group descriptor table.
    pub const fn descriptor_table_blocks(self) -> u32 {
        self.descriptor_table_blocks
    }

    /// Returns the number of inode table blocks in each group.
    pub const fn inode_table_blocks_per_group(self) -> u32 {
        self.inode_table_blocks_per_group
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SystemZone {
    start: u64,
    end: u64,
    owner: Option<InodeNumber>,
}

impl SystemZone {
    fn new(
        start: u64,
        count: u64,
        owner: Option<InodeNumber>,
        block_count: u64,
    ) -> Ext4Result<Self> {
        if count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        let end = start.checked_add(count).ok_or(Ext4Error::Overflow)?;
        if end > block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        Ok(Self { start, end, owner })
    }
}

/// Location selected from the ext4 journal fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalLocation {
    /// The filesystem has no journal.
    None,
    /// The journal is stored in a reserved inode.
    Internal { inode: InodeNumber },
    /// The journal is stored on another block device.
    External { dev: NonZeroU32, uuid: [u8; 16] },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JournalExtent {
    logical_start: u32,
    physical_start: u64,
    len: u32,
}

pub(crate) struct InternalJournal {
    pub(crate) superblock: JournalSuperblock,
    extents: Vec<JournalExtent>,
    pub(crate) block_count: u32,
}

impl JournalBlockMapper for InternalJournal {
    fn map_journal_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock> {
        let logical = block.get();
        if logical >= self.block_count {
            return Err(Ext4Error::OutOfBounds);
        }
        let index = self
            .extents
            .partition_point(|extent| extent.logical_start <= logical);
        let extent = index
            .checked_sub(1)
            .and_then(|index| self.extents.get(index))
            .ok_or(Ext4Error::OutOfBounds)?;
        let offset = logical
            .checked_sub(extent.logical_start)
            .filter(|offset| *offset < extent.len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let physical = extent
            .physical_start
            .checked_add(u64::from(offset))
            .ok_or(Ext4Error::Overflow)?;
        Ok(FilesystemBlock::new(physical))
    }
}

impl InternalJournal {
    pub(crate) fn validate_physical_bounds(&self, filesystem_block_count: u64) -> Ext4Result<()> {
        let mut next_logical = 0u32;
        for extent in &self.extents {
            if extent.len == 0 || extent.logical_start != next_logical {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
            next_logical = next_logical
                .checked_add(extent.len)
                .ok_or(Ext4Error::Overflow)?;
            let physical_end = extent
                .physical_start
                .checked_add(u64::from(extent.len))
                .ok_or(Ext4Error::Overflow)?;
            if physical_end > filesystem_block_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
        }
        // Linux JBD2 permits the backing inode to be larger than `s_maxlen`;
        // only the journal-addressable prefix must be fully mapped.
        if next_logical < self.block_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        Ok(())
    }
}

/// A validated ext4 filesystem mount core.
pub struct Ext4Filesystem {
    pub(crate) device: Arc<FilesystemDevice>,
    pub(crate) metadata_io: Ext4MetadataIo,
    pub(crate) journal: Option<Arc<MountedJournal>>,
    pub(crate) superblock: Superblock,
    pub(crate) layout: FilesystemLayout,
    pub(crate) groups: Vec<BlockGroupDescriptor>,
    pub(crate) block_free_extent_caches: Vec<Option<BlockGroupFreeExtentCache>>,
    system_zones: Vec<SystemZone>,
}

pub(crate) struct Ext4Recovery {
    pub(crate) filesystem: Ext4Filesystem,
}

pub(crate) struct JournalMarkedEmpty {
    pub(crate) report: JournalReplayReport,
}

pub(crate) struct Ext4RecoveryCleared {
    pub(crate) report: Ext4RecoveryReport,
}

impl Ext4RecoveryCleared {
    pub(crate) const fn into_report(self) -> Ext4RecoveryReport {
        self.report
    }
}

impl Ext4Filesystem {
    /// Reads and validates the primary ext4 superblock and group descriptor table.
    pub fn mount(device: Arc<dyn BlockDevice>) -> Ext4Result<Self> {
        Self::open(device, false)
    }

    /// Replays a filesystem journal and cleans the legacy orphan list.
    ///
    /// This entry point performs metadata writes. It is intentionally separate
    /// from [`mount`](Self::mount), which keeps the read-only mount path from
    /// modifying storage implicitly. A filesystem can have a clean journal but
    /// still contain legacy orphan entries left by a committed unlink or
    /// truncate. In that case this method performs journaled orphan cleanup and
    /// returns `Ok(None)` because no journal replay report was produced.
    pub fn recover(device: Arc<dyn BlockDevice>) -> Ext4Result<Option<Ext4RecoveryReport>> {
        Ext4Recovery::open(device)?.replay()
    }

    pub(crate) fn open(device: Arc<dyn BlockDevice>, allow_recovery: bool) -> Ext4Result<Self> {
        let mut superblock_bytes = [0; superblock::SUPERBLOCK_SIZE];
        FilesystemDevice::read_bytes(
            device.as_ref(),
            superblock::SUPERBLOCK_OFFSET,
            &mut superblock_bytes,
        )?;
        let superblock = Superblock::decode(&superblock_bytes)?;
        let layout = FilesystemLayout::derive(&superblock)?;
        let filesystem_device = Arc::new(FilesystemDevice::open(
            device,
            usize::try_from(layout.block_size).map_err(|_| Ext4Error::Overflow)?,
            layout.block_count,
        )?);
        let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());

        let descriptor_bytes = usize::try_from(layout.group_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(usize::from(layout.descriptor_size))
            .ok_or(Ext4Error::Overflow)?;
        let block_size = usize::try_from(layout.block_size).map_err(|_| Ext4Error::Overflow)?;
        let descriptor_blocks = descriptor_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let table_len = descriptor_blocks
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        let mut table = vec![0; table_len];
        for block_index in 0..descriptor_blocks {
            let physical = layout
                .descriptor_table_start
                .get()
                .checked_add(u64::try_from(block_index).map_err(|_| Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)?;
            let buffer = metadata_io.read_block(FilesystemBlock::new(physical))?;
            let start = block_index
                .checked_mul(block_size)
                .ok_or(Ext4Error::Overflow)?;
            let end = start.checked_add(block_size).ok_or(Ext4Error::Overflow)?;
            table[start..end].copy_from_slice(buffer.as_ref());
        }

        let mut groups = Vec::with_capacity(
            usize::try_from(layout.group_count).map_err(|_| Ext4Error::Overflow)?,
        );
        for group in 0..layout.group_count {
            let start = usize::try_from(group)
                .map_err(|_| Ext4Error::Overflow)?
                .checked_mul(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let end = start
                .checked_add(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let encoded = table.get(start..end).ok_or(Ext4Error::OutOfBounds)?;
            let descriptor =
                BlockGroupDescriptor::decode(encoded, superblock.features().has_64bit())?;

            if superblock.features().has_metadata_checksum() {
                let computed =
                    checksum::group_descriptor_checksum(encoded, group, superblock.checksum_seed())
                        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                if computed != descriptor.checksum() {
                    return Err(Ext4Error::ChecksumMismatch {
                        target: ChecksumTarget::BlockGroup(group),
                        expected: u32::from(computed),
                        actual: u32::from(descriptor.checksum()),
                    });
                }
            }
            if !superblock.features().has_metadata_checksum()
                && superblock
                    .features()
                    .read_only_compat()
                    .contains(features::ReadOnlyCompatFeatures::GDT_CSUM)
            {
                // TODO: Implement the legacy UUID-based CRC16 GDT checksum.
                return Err(Ext4Error::UnsupportedFeature {
                    class: crate::FeatureClass::ReadOnlyCompatible,
                    bits: features::ReadOnlyCompatFeatures::GDT_CSUM.bits(),
                });
            }

            validate_group(&superblock, &layout, group, &descriptor)?;
            groups.push(descriptor);
        }

        let mut filesystem = Self {
            device: filesystem_device,
            metadata_io,
            journal: None,
            superblock,
            layout,
            block_free_extent_caches: vec![None; groups.len()],
            groups,
            system_zones: Vec::new(),
        };
        filesystem.build_system_zones()?;
        let journal_location = filesystem.journal_location()?;
        if filesystem.superblock.features().needs_recovery() && !allow_recovery {
            return Err(Ext4Error::NeedsRecovery);
        }
        if !allow_recovery
            && (filesystem.orphan_head().is_some()
                || filesystem.superblock.features().has_orphan_present())
        {
            return Err(Ext4Error::NeedsRecovery);
        }
        filesystem.journal = match journal_location {
            JournalLocation::None => None,
            JournalLocation::Internal { inode } => Some(MountedJournal::new(
                filesystem.load_internal_journal(inode)?,
                filesystem.layout.block_count,
            )?),
            JournalLocation::External { .. } => {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalJournal));
            }
        };
        Ok(filesystem)
    }

    /// Returns the decoded primary superblock.
    pub const fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// Returns the immutable derived layout.
    pub const fn layout(&self) -> FilesystemLayout {
        self.layout
    }

    /// Returns a validated block group descriptor.
    pub fn group(&self, group: BlockGroupNumber) -> Option<&BlockGroupDescriptor> {
        self.groups.get(group.get() as usize)
    }

    /// Returns all validated block group descriptors.
    pub fn groups(&self) -> &[BlockGroupDescriptor] {
        &self.groups
    }

    /// Returns public status for the internal JBD2 journal, when present.
    pub fn journal_status(&self) -> Option<JournalStatus> {
        self.journal
            .as_ref()
            .map(|journal| JournalStatus::from(&journal.superblock()))
    }

    /// Returns the filesystem block containing the internal journal superblock.
    ///
    /// This is a diagnostic/mount test helper that avoids exposing JBD2 block
    /// address types as public KExt4 API.
    pub fn journal_superblock_block(&self) -> Ext4Result<Option<FilesystemBlock>> {
        match self.journal.as_ref() {
            Some(_) => JournalBlockMapper::map_journal_block(self, JournalBlock::new(0)).map(Some),
            None => Ok(None),
        }
    }

    /// Returns the physical block for one internal journal block.
    pub fn map_journal_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock> {
        JournalBlockMapper::map_journal_block(self, block)
    }

    fn scan_internal_journal(&self) -> Ext4Result<JournalLogScan> {
        let journal = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        scan_journal(&journal.superblock(), self)
    }

    pub(crate) fn replay_internal_journal_updates(&self) -> Ext4Result<JournalReplayApplied> {
        let scan = self.scan_internal_journal()?;
        replay_scanned_journal(self, self.device.as_ref(), &scan, self.device.block_size())
    }

    /// Reclaims up to `limit` metadata blocks with no active readers.
    pub fn reclaim_metadata_blocks(&self, limit: usize) -> usize {
        self.metadata_io.reclaim_unused(limit)
    }

    /// Returns ext4 filesystem statistics without depending on VFS types.
    pub fn statfs(&self) -> Ext4Result<Ext4StatFs> {
        let (free_blocks, free_inodes) =
            self.groups
                .iter()
                .fold((0u64, 0u64), |(blocks, inodes), group| {
                    (
                        blocks + u64::from(group.free_blocks_count()),
                        inodes + u64::from(group.free_inodes_count()),
                    )
                });
        let overhead = self.statfs_overhead_blocks()?;
        let blocks = self
            .superblock
            .blocks_count()
            .checked_sub(overhead)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
        let blocks_available = free_blocks.saturating_sub(self.superblock.reserved_blocks_count());

        Ok(Ext4StatFs {
            block_size: self.superblock.block_size(),
            fragment_size: self.superblock.block_size(),
            blocks,
            blocks_free: free_blocks,
            blocks_available,
            files: u64::from(self.superblock.inodes_count()),
            files_free: free_inodes,
            max_name_len: crate::disk::dir::DIRENT_NAME_MAX as u32,
        })
    }

    /// Returns the free-block budget available after ext4 reserved blocks.
    ///
    /// Allocation and release paths update the primary superblock counter in
    /// the same metadata operation as their group descriptor. Delayed
    /// allocation admission can therefore use this constant-time aggregate
    /// instead of folding every block-group descriptor as `statfs()` does.
    pub fn blocks_available_for_reservation(&self) -> u64 {
        self.superblock
            .free_blocks_count()
            .saturating_sub(self.superblock.reserved_blocks_count())
    }

    /// Reads complete filesystem blocks without exposing metadata mutation.
    pub fn read_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        output: &mut [u8],
    ) -> Ext4Result<()> {
        self.device.read_blocks(start, block_count, output)
    }

    pub(crate) fn write_contiguous_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        input: &[u8],
    ) -> Ext4Result<()> {
        self.device
            .write_contiguous_blocks(start, block_count, input)
    }

    pub(crate) fn flush_device(&self) -> Ext4Result<()> {
        self.device.flush()
    }

    pub(crate) fn read_metadata_block(&self, block: FilesystemBlock) -> Ext4Result<MetadataBuffer> {
        self.metadata_io.read_block(block)
    }

    pub(crate) fn reload_mutable_metadata_state(&mut self) -> Ext4Result<()> {
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        let superblock_buffer = self.read_metadata_block(superblock_block)?;
        let superblock_bytes = superblock_buffer
            .as_ref()
            .get(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let superblock = Superblock::decode(superblock_bytes)?;
        let layout = FilesystemLayout::derive(&superblock)?;
        if layout != self.layout {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        let groups = self.decode_group_descriptors(&superblock, layout)?;
        self.superblock = superblock;
        self.groups = groups;
        self.reset_block_allocation_caches();
        Ok(())
    }

    fn decode_group_descriptors(
        &self,
        superblock: &Superblock,
        layout: FilesystemLayout,
    ) -> Ext4Result<Vec<BlockGroupDescriptor>> {
        let descriptor_bytes = usize::try_from(layout.group_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(usize::from(layout.descriptor_size))
            .ok_or(Ext4Error::Overflow)?;
        let block_size = usize::try_from(layout.block_size).map_err(|_| Ext4Error::Overflow)?;
        let descriptor_blocks = descriptor_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let table_len = descriptor_blocks
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        let mut table = vec![0; table_len];
        for block_index in 0..descriptor_blocks {
            let physical = layout
                .descriptor_table_start
                .get()
                .checked_add(u64::try_from(block_index).map_err(|_| Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)?;
            let buffer = self.read_metadata_block(FilesystemBlock::new(physical))?;
            let start = block_index
                .checked_mul(block_size)
                .ok_or(Ext4Error::Overflow)?;
            let end = start.checked_add(block_size).ok_or(Ext4Error::Overflow)?;
            table[start..end].copy_from_slice(buffer.as_ref());
        }

        let mut groups = Vec::with_capacity(
            usize::try_from(layout.group_count).map_err(|_| Ext4Error::Overflow)?,
        );
        for group in 0..layout.group_count {
            let start = usize::try_from(group)
                .map_err(|_| Ext4Error::Overflow)?
                .checked_mul(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let end = start
                .checked_add(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let encoded = table.get(start..end).ok_or(Ext4Error::OutOfBounds)?;
            let descriptor =
                BlockGroupDescriptor::decode(encoded, superblock.features().has_64bit())?;

            if superblock.features().has_metadata_checksum() {
                let computed =
                    checksum::group_descriptor_checksum(encoded, group, superblock.checksum_seed())
                        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                if computed != descriptor.checksum() {
                    return Err(Ext4Error::ChecksumMismatch {
                        target: ChecksumTarget::BlockGroup(group),
                        expected: u32::from(computed),
                        actual: u32::from(descriptor.checksum()),
                    });
                }
            }
            validate_group(superblock, &layout, group, &descriptor)?;
            groups.push(descriptor);
        }
        Ok(groups)
    }

    pub(crate) fn is_inode_physical_block_valid(
        &self,
        inode: InodeNumber,
        block: u64,
        count: u64,
    ) -> bool {
        is_inode_physical_block_valid(
            self.superblock.first_data_block(),
            self.superblock.blocks_count(),
            &self.system_zones,
            inode,
            block,
            count,
        )
    }

    fn build_system_zones(&mut self) -> Ext4Result<()> {
        for group in 0..self.layout.group_count {
            let group_first = self.group_first_block(group)?;
            let base_metadata_blocks = self.base_metadata_blocks(group)?;
            if base_metadata_blocks != 0 {
                self.add_system_zone(group_first, base_metadata_blocks, None)?;
            }

            let (block_bitmap, inode_bitmap, inode_table) = {
                let descriptor = self
                    .group(BlockGroupNumber::new(group))
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
                (
                    descriptor.block_bitmap(),
                    descriptor.inode_bitmap(),
                    descriptor.inode_table(),
                )
            };
            self.add_system_zone(block_bitmap, 1, None)?;
            self.add_system_zone(inode_bitmap, 1, None)?;
            self.add_system_zone(
                inode_table,
                u64::from(self.layout.inode_table_blocks_per_group),
                None,
            )?;
        }

        Ok(())
    }

    fn journal_location(&self) -> Ext4Result<JournalLocation> {
        let has_journal = self.superblock.features().has_journal();
        let fields = self.superblock.journal();
        select_journal_location(has_journal, fields.inode(), fields.device(), fields.uuid())
    }

    fn load_internal_journal(&mut self, inode_number: InodeNumber) -> Ext4Result<InternalJournal> {
        let journal_inode = self.internal_inode(inode_number)?;
        if journal_inode.kind() != InodeKind::RegularFile {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        let block_size = u64::from(self.layout.block_size);
        if journal_inode.size() % block_size != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        let journal_blocks = journal_inode.size() / block_size;
        let journal_blocks = u32::try_from(journal_blocks).map_err(|_| Ext4Error::Overflow)?;
        if journal_blocks == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }

        let extents = collect_journal_extents(journal_blocks, |logical| {
            self.map_blocks(&journal_inode, LogicalBlock::new(u64::from(logical)))
        })?;
        for extent in &extents {
            self.add_system_zone(
                extent.physical_start,
                u64::from(extent.len),
                Some(inode_number),
            )?;
        }

        let superblock = JournalSuperblock::decode(
            self.read_metadata_block(FilesystemBlock::new(extents[0].physical_start))?
                .as_ref(),
            self.layout.block_size,
            journal_blocks,
            self.superblock.uuid(),
        )?;
        let block_count = superblock.max_blocks();
        let journal = InternalJournal {
            superblock,
            extents,
            block_count,
        };
        debug_assert_eq!(
            journal.map_journal_block(JournalBlock::new(0))?,
            FilesystemBlock::new(journal.extents[0].physical_start)
        );
        Ok(journal)
    }

    pub(crate) fn add_system_zone(
        &mut self,
        start: u64,
        count: u64,
        owner: Option<InodeNumber>,
    ) -> Ext4Result<()> {
        let zone = SystemZone::new(start, count, owner, self.layout.block_count)?;
        let index = self
            .system_zones
            .partition_point(|entry| entry.start < zone.start);

        if index > 0 {
            let previous = self.system_zones[index - 1];
            if previous.end > zone.start {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
            }
            if previous.end == zone.start && previous.owner == zone.owner {
                self.system_zones[index - 1].end = zone.end;
                if index < self.system_zones.len() {
                    let next = self.system_zones[index];
                    if zone.end > next.start {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
                    }
                    if zone.end == next.start && next.owner == zone.owner {
                        let next = self.system_zones.remove(index);
                        self.system_zones[index - 1].end = next.end;
                    }
                }
                return Ok(());
            }
        }

        if index < self.system_zones.len() {
            let next = self.system_zones[index];
            if zone.end > next.start {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
            }
            if zone.end == next.start && zone.owner == next.owner {
                self.system_zones[index].start = zone.start;
                return Ok(());
            }
        }

        self.system_zones.insert(index, zone);
        Ok(())
    }

    pub(crate) fn remove_system_zone(
        &mut self,
        start: u64,
        count: u64,
        owner: Option<InodeNumber>,
    ) -> Ext4Result<()> {
        let zone = SystemZone::new(start, count, owner, self.layout.block_count)?;
        let index = self
            .system_zones
            .partition_point(|entry| entry.end <= zone.start);
        let existing = self
            .system_zones
            .get(index)
            .copied()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
        if existing.owner != zone.owner || existing.start > zone.start || existing.end < zone.end {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }

        match (existing.start == zone.start, existing.end == zone.end) {
            (true, true) => {
                self.system_zones.remove(index);
            }
            (true, false) => {
                self.system_zones[index].start = zone.end;
            }
            (false, true) => {
                self.system_zones[index].end = zone.start;
            }
            (false, false) => {
                self.system_zones[index].end = zone.start;
                self.system_zones.insert(
                    index + 1,
                    SystemZone {
                        start: zone.end,
                        end: existing.end,
                        owner: existing.owner,
                    },
                );
            }
        }
        Ok(())
    }

    fn base_metadata_blocks(&self, group: u32) -> Ext4Result<u64> {
        if !self.group_has_super(group)? {
            return Ok(0);
        }
        u64::from(1u32)
            .checked_add(u64::from(self.layout.descriptor_table_blocks))
            .and_then(|blocks| blocks.checked_add(u64::from(self.superblock.reserved_gdt_blocks())))
            .ok_or(Ext4Error::Overflow)
    }

    fn group_has_super(&self, group: u32) -> Ext4Result<bool> {
        if group == 0 {
            return Ok(true);
        }
        let features = self.superblock.features();
        if features.has_sparse_super2() {
            return Ok(self.superblock.backup_groups().contains(&group));
        }
        if group <= 1 || !features.has_sparse_super() {
            return Ok(true);
        }
        if group.is_multiple_of(2) {
            return Ok(false);
        }
        Ok(test_root(group, 3) || test_root(group, 5) || test_root(group, 7))
    }

    fn group_first_block(&self, group: u32) -> Ext4Result<u64> {
        u64::from(group)
            .checked_mul(u64::from(self.superblock.blocks_per_group()))
            .and_then(|offset| offset.checked_add(u64::from(self.superblock.first_data_block())))
            .ok_or(Ext4Error::Overflow)
    }

    pub(crate) fn block_group_range(&self, group: BlockGroupNumber) -> Ext4Result<BlockGroupRange> {
        if group.get() >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        let first = self.group_first_block(group.get())?;
        let end = first
            .checked_add(u64::from(self.superblock.blocks_per_group()))
            .ok_or(Ext4Error::Overflow)?
            .min(self.superblock.blocks_count());
        let block_count = u32::try_from(end.checked_sub(first).ok_or(Ext4Error::Overflow)?)
            .map_err(|_| Ext4Error::Overflow)?;
        BlockGroupRange::new(group, FilesystemBlock::new(first), block_count)
    }

    pub(crate) fn block_bitmap_checksum_bytes(&self) -> Ext4Result<usize> {
        bitmap_checksum_bytes(
            self.superblock.clusters_per_group(),
            CorruptKind::InvalidBlockGroupGeometry,
        )
    }

    pub(crate) fn inode_bitmap_checksum_bytes(&self) -> Ext4Result<usize> {
        bitmap_checksum_bytes(
            self.superblock.inodes_per_group(),
            CorruptKind::InvalidInodeGeometry,
        )
    }

    pub(crate) fn block_group_for_block(
        &self,
        block: FilesystemBlock,
    ) -> Ext4Result<BlockGroupNumber> {
        let first_data = u64::from(self.superblock.first_data_block());
        if block.get() < first_data || block.get() >= self.superblock.blocks_count() {
            return Err(Ext4Error::OutOfBounds);
        }
        let group = (block.get() - first_data) / u64::from(self.superblock.blocks_per_group());
        let group = u32::try_from(group).map_err(|_| Ext4Error::Overflow)?;
        if group >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        Ok(BlockGroupNumber::new(group))
    }

    pub(crate) fn block_allocation_start_group(
        &self,
        goal: Option<FilesystemBlock>,
    ) -> Ext4Result<BlockGroupNumber> {
        match goal {
            Some(goal)
                if goal.get() >= u64::from(self.superblock.first_data_block())
                    && goal.get() < self.superblock.blocks_count() =>
            {
                self.block_group_for_block(goal)
            }
            Some(_) | None => Ok(BlockGroupNumber::new(0)),
        }
    }

    pub(crate) fn inode_group_range(&self, group: BlockGroupNumber) -> Ext4Result<InodeGroupRange> {
        if group.get() >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        let first_inode = group
            .get()
            .checked_mul(self.superblock.inodes_per_group())
            .and_then(|offset| offset.checked_add(1))
            .ok_or(Ext4Error::Overflow)?;
        let remaining = self
            .superblock
            .inodes_count()
            .checked_sub(first_inode - 1)
            .ok_or(Ext4Error::OutOfBounds)?;
        let inode_count = self.superblock.inodes_per_group().min(remaining);
        InodeGroupRange::new(group, InodeNumber::new(first_inode), inode_count)
    }

    pub(crate) fn block_group_for_inode(&self, inode: InodeNumber) -> Ext4Result<BlockGroupNumber> {
        if inode.get() == 0 || inode.get() > self.superblock.inodes_count() {
            return Err(Ext4Error::OutOfBounds);
        }
        let group = (inode.get() - 1) / self.superblock.inodes_per_group();
        if group >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        Ok(BlockGroupNumber::new(group))
    }

    pub(crate) fn find_group_orlov(
        &self,
        parent: Option<InodeNumber>,
        child_name: Option<&[u8]>,
    ) -> Ext4Result<BlockGroupNumber> {
        if self.layout.group_count == 0 {
            return Err(Ext4Error::NoSpace);
        }
        let totals = self.inode_allocation_totals();
        if totals.free_inodes == 0 {
            return Err(Ext4Error::NoSpace);
        }
        let parent_group = parent
            .map(|inode| self.block_group_for_inode(inode))
            .transpose()?;
        let flex_count = u64::from(self.flex_group_count());
        let avg_free_inodes = totals.free_inodes / flex_count;
        let avg_free_blocks = totals.free_blocks / flex_count;
        let is_top_level_directory = self.is_top_level_directory_parent(parent);
        let start_flex = if is_top_level_directory {
            self.orlov_top_level_start_flex(child_name)
        } else {
            parent_group
                .map(|group| self.flex_group_index(group))
                .unwrap_or(0)
        };

        if is_top_level_directory {
            if let Some(group) =
                self.find_top_level_directory_group(start_flex, avg_free_inodes, avg_free_blocks)?
            {
                return Ok(group);
            }
        } else if let Some(group) =
            self.find_child_directory_group(start_flex, totals, avg_free_inodes, avg_free_blocks)?
        {
            return Ok(group);
        }

        let start_group = parent_group.unwrap_or(BlockGroupNumber::new(0));
        self.find_inode_group_with_min_free_inodes(start_group, avg_free_inodes)
            .or_else(|| self.find_inode_group_with_min_free_inodes(start_group, 1))
            .ok_or(Ext4Error::NoSpace)
    }

    pub(crate) fn find_group_other(
        &self,
        parent: Option<InodeNumber>,
    ) -> Ext4Result<BlockGroupNumber> {
        if self.layout.group_count == 0 {
            return Err(Ext4Error::NoSpace);
        }
        let start = match parent {
            Some(parent) => self.block_group_for_inode(parent)?,
            None => BlockGroupNumber::new(0),
        };

        if self.superblock.features().has_flex_bg() {
            if let Some(group) =
                self.first_data_inode_group_in_flex(self.flex_group_index(start))?
            {
                return Ok(group);
            }
        } else if self.group_has_free_inode_and_block(start)? {
            return Ok(start);
        }

        let mut group = start.get();
        let mut probe = 1u32;
        while probe < self.layout.group_count {
            group = group.checked_add(probe).ok_or(Ext4Error::Overflow)? % self.layout.group_count;
            let candidate = BlockGroupNumber::new(group);
            if self.group_has_free_inode_and_block(candidate)? {
                return Ok(candidate);
            }
            probe = probe.checked_shl(1).unwrap_or(self.layout.group_count);
        }

        self.find_inode_group_with_min_free_inodes(start, 1)
            .ok_or(Ext4Error::NoSpace)
    }

    fn inode_allocation_totals(&self) -> InodeAllocationTotals {
        self.groups.iter().fold(
            InodeAllocationTotals {
                free_inodes: 0,
                free_blocks: 0,
                used_directories: 0,
            },
            |totals, descriptor| InodeAllocationTotals {
                free_inodes: totals
                    .free_inodes
                    .saturating_add(u64::from(descriptor.free_inodes_count())),
                free_blocks: totals
                    .free_blocks
                    .saturating_add(u64::from(descriptor.free_blocks_count())),
                used_directories: totals
                    .used_directories
                    .saturating_add(u64::from(descriptor.used_directories_count())),
            },
        )
    }

    fn is_top_level_directory_parent(&self, parent: Option<InodeNumber>) -> bool {
        parent.is_none() || parent.is_some_and(|inode| inode == InodeNumber::new(2))
    }

    fn orlov_top_level_start_flex(&self, child_name: Option<&[u8]>) -> u32 {
        let flex_count = self.flex_group_count();
        if flex_count <= 1 {
            return 0;
        }
        self.orlov_child_name_hash(child_name) % flex_count
    }

    fn orlov_child_name_hash(&self, child_name: Option<&[u8]>) -> u32 {
        let Some(name) = child_name.filter(|name| !name.is_empty()) else {
            return self.superblock.checksum_seed();
        };
        if let Ok(hash) = directory_hash(
            name,
            self.superblock.default_hash_version(),
            self.superblock.hash_seed(),
        ) {
            return hash.major();
        }

        let mut hash = 0x811c_9dc5_u32 ^ self.superblock.checksum_seed();
        for byte in name {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193).rotate_left(5);
        }
        hash
    }

    fn find_top_level_directory_group(
        &self,
        start_flex: u32,
        avg_free_inodes: u64,
        avg_free_blocks: u64,
    ) -> Ext4Result<Option<BlockGroupNumber>> {
        let mut best = None;
        for flex in self.flex_scan_order(start_flex) {
            let stats = self.flex_group_stats(flex)?;
            if stats.free_inodes < avg_free_inodes || stats.free_blocks < avg_free_blocks {
                continue;
            }
            let Some(group) = self.best_directory_group_in_flex(flex)? else {
                continue;
            };
            best = match best {
                Some((_, best_used_dirs)) if best_used_dirs <= stats.used_directories => best,
                _ => Some((group, stats.used_directories)),
            };
        }
        Ok(best.map(|(group, _)| group))
    }

    fn find_child_directory_group(
        &self,
        start_flex: u32,
        totals: InodeAllocationTotals,
        avg_free_inodes: u64,
        avg_free_blocks: u64,
    ) -> Ext4Result<Option<BlockGroupNumber>> {
        let flex_size = u64::from(self.flex_group_size());
        let flex_count = u64::from(self.flex_group_count());
        let max_dirs = (totals.used_directories / flex_count)
            .saturating_add(u64::from(self.superblock.inodes_per_group()) * flex_size / 16);
        let min_inodes = avg_free_inodes
            .saturating_sub(u64::from(self.superblock.inodes_per_group()) * flex_size / 4);
        let min_blocks = avg_free_blocks
            .saturating_sub(u64::from(self.superblock.blocks_per_group()) * flex_size / 4);

        for flex in self.flex_scan_order(start_flex) {
            let stats = self.flex_group_stats(flex)?;
            if stats.used_directories >= max_dirs
                || stats.free_inodes < min_inodes
                || stats.free_blocks < min_blocks
            {
                continue;
            }
            if let Some(group) = self.best_directory_group_in_flex(flex)? {
                return Ok(Some(group));
            }
        }
        Ok(None)
    }

    fn find_inode_group_with_min_free_inodes(
        &self,
        start: BlockGroupNumber,
        min_free_inodes: u64,
    ) -> Option<BlockGroupNumber> {
        self.group_scan_order(start).ok()?.find(|group| {
            self.group_descriptor(*group)
                .map(|descriptor| u64::from(descriptor.free_inodes_count()) >= min_free_inodes)
                .unwrap_or(false)
        })
    }

    fn first_data_inode_group_in_flex(&self, flex: u32) -> Ext4Result<Option<BlockGroupNumber>> {
        for group in self.flex_group_range(flex) {
            if self.group_has_free_inode_and_block(group)? {
                return Ok(Some(group));
            }
        }
        Ok(None)
    }

    fn best_directory_group_in_flex(&self, flex: u32) -> Ext4Result<Option<BlockGroupNumber>> {
        let mut best = None;
        for group in self.flex_group_range(flex) {
            let descriptor = self.group_descriptor(group)?;
            if descriptor.free_inodes_count() == 0 || descriptor.free_blocks_count() == 0 {
                continue;
            }
            best = match best {
                Some((_, best_used_dirs))
                    if best_used_dirs <= descriptor.used_directories_count() =>
                {
                    best
                }
                _ => Some((group, descriptor.used_directories_count())),
            };
        }
        Ok(best.map(|(group, _)| group))
    }

    fn group_has_free_inode_and_block(&self, group: BlockGroupNumber) -> Ext4Result<bool> {
        let descriptor = self.group_descriptor(group)?;
        Ok(descriptor.free_inodes_count() > 0 && descriptor.free_blocks_count() > 0)
    }

    fn group_descriptor(&self, group: BlockGroupNumber) -> Ext4Result<&BlockGroupDescriptor> {
        self.groups
            .get(usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::OutOfBounds)
    }

    fn flex_group_index(&self, group: BlockGroupNumber) -> u32 {
        group.get() / self.flex_group_size()
    }

    fn flex_group_size(&self) -> u32 {
        if !self.superblock.features().has_flex_bg() {
            return 1;
        }
        let size = 1u32.checked_shl(u32::from(self.superblock.log_groups_per_flex()));
        debug_assert!(
            size.is_some(),
            "validated flex_bg log_groups_per_flex must fit u32 shift"
        );
        size.unwrap_or(1)
    }

    fn flex_group_count(&self) -> u32 {
        let flex_size = self.flex_group_size();
        self.layout
            .group_count
            .saturating_add(flex_size - 1)
            .checked_div(flex_size)
            .unwrap_or(1)
            .max(1)
    }

    fn flex_scan_order(&self, start: u32) -> impl Iterator<Item = u32> + '_ {
        let flex_count = self.flex_group_count();
        (0..flex_count).map(move |offset| {
            ((u64::from(start) + u64::from(offset)) % u64::from(flex_count)) as u32
        })
    }

    fn flex_group_range(&self, flex: u32) -> impl Iterator<Item = BlockGroupNumber> + '_ {
        let flex_size = self.flex_group_size();
        let start = flex.saturating_mul(flex_size);
        let end = start.saturating_add(flex_size).min(self.layout.group_count);
        (start..end).map(BlockGroupNumber::new)
    }

    fn flex_group_stats(&self, flex: u32) -> Ext4Result<FlexGroupStats> {
        let mut stats = FlexGroupStats {
            free_inodes: 0,
            free_blocks: 0,
            used_directories: 0,
        };
        for group in self.flex_group_range(flex) {
            let descriptor = self.group_descriptor(group)?;
            stats.free_inodes = stats
                .free_inodes
                .saturating_add(u64::from(descriptor.free_inodes_count()));
            stats.free_blocks = stats
                .free_blocks
                .saturating_add(u64::from(descriptor.free_blocks_count()));
            stats.used_directories = stats
                .used_directories
                .saturating_add(u64::from(descriptor.used_directories_count()));
        }
        Ok(stats)
    }

    pub(crate) fn group_scan_order(&self, start: BlockGroupNumber) -> Ext4Result<GroupScanOrder> {
        if self.layout.group_count == 0 || start.get() >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        Ok(GroupScanOrder {
            group_count: self.layout.group_count,
            start: start.get(),
            offset: 0,
        })
    }

    pub(crate) fn group_descriptor_location(
        &self,
        group: BlockGroupNumber,
    ) -> Ext4Result<(FilesystemBlock, usize, usize)> {
        if group.get() >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        let descriptor_offset = usize::try_from(group.get())
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(usize::from(self.layout.descriptor_size))
            .ok_or(Ext4Error::Overflow)?;
        let block_size =
            usize::try_from(self.layout.block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_offset = descriptor_offset / block_size;
        let block = self
            .layout
            .descriptor_table_start
            .get()
            .checked_add(u64::try_from(block_offset).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        Ok((
            FilesystemBlock::new(block),
            descriptor_offset % block_size,
            usize::from(self.layout.descriptor_size),
        ))
    }

    pub(crate) fn primary_superblock_location(
        &self,
    ) -> Ext4Result<(FilesystemBlock, usize, usize)> {
        let block_size =
            u64::try_from(self.device.block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block = superblock::SUPERBLOCK_OFFSET / block_size;
        let offset = usize::try_from(superblock::SUPERBLOCK_OFFSET % block_size)
            .map_err(|_| Ext4Error::Overflow)?;
        Ok((
            FilesystemBlock::new(block),
            offset,
            superblock::SUPERBLOCK_SIZE,
        ))
    }

    pub(crate) fn is_system_zone_block(&self, block: FilesystemBlock) -> bool {
        let block = block.get();
        let index = self.system_zones.partition_point(|zone| zone.end <= block);
        self.system_zones
            .get(index)
            .is_some_and(|zone| zone.start <= block && block < zone.end)
    }

    pub(crate) fn is_inode_owned_system_zone_block(
        &self,
        block: FilesystemBlock,
        inode: InodeNumber,
    ) -> bool {
        let block = block.get();
        let index = self.system_zones.partition_point(|zone| zone.end <= block);
        self.system_zones.get(index).is_some_and(|zone| {
            zone.start <= block && block < zone.end && zone.owner == Some(inode)
        })
    }

    pub(crate) fn is_reserved_inode(&self, inode: InodeNumber) -> bool {
        inode.get() != 0 && inode.get() < self.superblock.first_inode()
    }

    fn statfs_overhead_blocks(&self) -> Ext4Result<u64> {
        let zones = self.system_zones.iter().try_fold(0u64, |blocks, zone| {
            blocks
                .checked_add(
                    zone.end
                        .checked_sub(zone.start)
                        .ok_or(Ext4Error::Overflow)?,
                )
                .ok_or(Ext4Error::Overflow)
        })?;
        u64::from(self.superblock.first_data_block())
            .checked_add(zones)
            .ok_or(Ext4Error::Overflow)
    }
}

pub(crate) struct GroupScanOrder {
    group_count: u32,
    start: u32,
    offset: u32,
}

impl Iterator for GroupScanOrder {
    type Item = BlockGroupNumber;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.group_count {
            return None;
        }
        let group = (self.start + self.offset) % self.group_count;
        self.offset += 1;
        Some(BlockGroupNumber::new(group))
    }
}

pub(crate) fn metadata_access_bytes(access: &MetadataWriteAccess) -> Ext4Result<Vec<u8>> {
    Ok(Vec::from(access.snapshot()?.as_ref()))
}

pub(crate) fn replace_metadata_access_bytes(
    access: &MetadataWriteAccess,
    bytes: Vec<u8>,
) -> Ext4Result<()> {
    access.replace_bytes(Arc::from(bytes.into_boxed_slice()))
}

pub(crate) fn bitmap_bit_capacity(bitmap: &[u8]) -> Ext4Result<u32> {
    let bits = bitmap.len().checked_mul(8).ok_or(Ext4Error::Overflow)?;
    u32::try_from(bits).map_err(|_| Ext4Error::Overflow)
}

pub(crate) fn ext4_mark_bitmap_end(
    valid_bits: u32,
    total_bits: u32,
    bitmap: &mut [u8],
) -> Ext4Result<()> {
    if valid_bits > total_bits {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
    }
    if total_bits > bitmap_bit_capacity(bitmap)? {
        return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
    }
    for bit in valid_bits..total_bits {
        set_ext4_bitmap_bit(bitmap, bit)?;
    }
    Ok(())
}

pub(crate) fn validate_ext4_bitmap_range_set(
    bitmap: &[u8],
    start_bit: u32,
    end_bit: u32,
    corrupt_kind: CorruptKind,
) -> Ext4Result<()> {
    if start_bit > end_bit {
        return Err(Ext4Error::Corrupt(corrupt_kind));
    }
    if end_bit > bitmap_bit_capacity(bitmap)? {
        return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
    }
    for bit in start_bit..end_bit {
        if !is_ext4_bitmap_bit_set(bitmap, bit)? {
            return Err(Ext4Error::Corrupt(corrupt_kind));
        }
    }
    Ok(())
}

pub(crate) fn set_ext4_bitmap_bit(bitmap: &mut [u8], bit_index: u32) -> Ext4Result<()> {
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext4Error::Overflow)?;
    let mask = 1u8 << (bit_index % 8);
    *bitmap.get_mut(byte).ok_or(Ext4Error::OutOfBounds)? |= mask;
    Ok(())
}

pub(crate) fn is_ext4_bitmap_bit_set(bitmap: &[u8], bit_index: u32) -> Ext4Result<bool> {
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext4Error::Overflow)?;
    let mask = 1u8 << (bit_index % 8);
    Ok(bitmap.get(byte).ok_or(Ext4Error::OutOfBounds)? & mask != 0)
}

pub(crate) fn count_clear_ext4_bitmap_bits(bitmap: &[u8], valid_bits: u32) -> Ext4Result<u32> {
    if valid_bits > bitmap_bit_capacity(bitmap)? {
        return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
    }
    let mut clear_bits = 0u32;
    for bit in 0..valid_bits {
        if !is_ext4_bitmap_bit_set(bitmap, bit)? {
            clear_bits = clear_bits.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
    }
    Ok(clear_bits)
}

pub(crate) fn ensure_metadata_credits(
    handle: &crate::jbd2::JournalHandle<'_>,
    required_credits: u32,
) -> Ext4Result<()> {
    if handle.remaining_credits() < required_credits {
        return Err(Ext4Error::InsufficientJournalCredits);
    }
    Ok(())
}

fn select_journal_location(
    has_journal: bool,
    inode: Option<NonZeroU32>,
    device: Option<NonZeroU32>,
    uuid: [u8; 16],
) -> Ext4Result<JournalLocation> {
    match (has_journal, inode, device) {
        (false, None, None) => Ok(JournalLocation::None),
        (false, ..) => Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal)),
        (true, Some(inode), None) => Ok(JournalLocation::Internal {
            inode: InodeNumber::new(inode.get()),
        }),
        (true, None, Some(dev)) => Ok(JournalLocation::External { dev, uuid }),
        (true, Some(_), Some(_)) | (true, None, None) => {
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        }
    }
}

fn collect_journal_extents(
    journal_blocks: u32,
    mut map: impl FnMut(u32) -> Ext4Result<BlockMapping>,
) -> Ext4Result<Vec<JournalExtent>> {
    let mut extents = Vec::new();
    let mut logical = 0u32;
    while logical < journal_blocks {
        match map(logical)? {
            BlockMapping::Mapped { physical, len } if len.get() != 0 => {
                let run_len = len.get().min(journal_blocks - logical);
                extents.push(JournalExtent {
                    logical_start: logical,
                    physical_start: physical.get(),
                    len: run_len,
                });
                logical = logical.checked_add(run_len).ok_or(Ext4Error::Overflow)?;
            }
            BlockMapping::Hole { .. }
            | BlockMapping::Unwritten { .. }
            | BlockMapping::Mapped { .. } => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
        }
    }
    Ok(extents)
}

fn test_root(mut group: u32, factor: u32) -> bool {
    loop {
        if group < factor {
            return false;
        }
        if group == factor {
            return true;
        }
        if !group.is_multiple_of(factor) {
            return false;
        }
        group /= factor;
    }
}

fn bitmap_checksum_bytes(bits: u32, corrupt_kind: CorruptKind) -> Ext4Result<usize> {
    if !bits.is_multiple_of(8) {
        return Err(Ext4Error::Corrupt(corrupt_kind));
    }
    usize::try_from(bits / 8).map_err(|_| Ext4Error::Overflow)
}

pub(crate) const fn ext4_bitmap_checksum_matches(
    calculated: u32,
    expected: u32,
    has_64bit_descriptor: bool,
) -> bool {
    if has_64bit_descriptor {
        calculated == expected
    } else {
        calculated as u16 == expected as u16
    }
}

fn is_inode_physical_block_valid(
    first_data_block: u32,
    blocks_count: u64,
    system_zones: &[SystemZone],
    inode: InodeNumber,
    block: u64,
    count: u64,
) -> bool {
    if count == 0 || block <= u64::from(first_data_block) {
        return false;
    }
    let Some(end) = block.checked_add(count) else {
        return false;
    };
    if end > blocks_count {
        return false;
    }
    let mut index = system_zones.partition_point(|zone| zone.end <= block);
    while let Some(zone) = system_zones.get(index) {
        if zone.start >= end {
            break;
        }
        if zone.owner != Some(inode) {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::{
        format, fs,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::Mutex,
    };

    use block::{Device, DeviceKind, DriverError, DriverResult};

    use super::*;
    use crate::{
        BlockCount, Ext4Inode, Ext4SyncIntent, LogicalBlock, PhysicalBlock,
        extent::ExtentMappingState,
        file::RegularWriteMetadata,
        inode::InodeInitialization,
        jbd2::{JournalCredits, JournalTransactions, TransactionId},
        mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    };

    const TEST_BLOCK_SIZE: usize = 4096;
    const TEST_BLOCK_COUNT: usize = 32;
    const TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT: usize = 2048;
    const TEST_FREE_BLOCKS: u32 = 26;
    const TEST_FREE_INODES: u32 = 22;
    const LINUX_IMAGE_DEVICE_BLOCK_SIZE: usize = 512;
    const TEST_EXT4_BG_INODE_UNINIT: u16 = 0x0001;
    const TEST_EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;

    struct TestDevice {
        bytes: Mutex<Vec<u8>>,
        flush_count: Mutex<usize>,
        fail_flush_at: Mutex<Option<usize>>,
    }

    struct LinuxImageDevice {
        bytes: Mutex<Vec<u8>>,
        flush_count: Mutex<usize>,
        fail_flush_at: Mutex<Option<usize>>,
    }

    #[derive(Clone, Copy)]
    struct AllocatorGroupSpec {
        free_blocks: u32,
        free_inodes: u32,
        used_directories: u32,
        flags: u16,
        block_bitmap: [u8; 4],
        inode_bitmap: [u8; 4],
    }

    impl TestDevice {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes: Mutex::new(bytes),
                flush_count: Mutex::new(0),
                fail_flush_at: Mutex::new(None),
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        fn flush_count(&self) -> usize {
            *self.flush_count.lock().unwrap()
        }

        fn fail_flush_at(&self, flush_count: usize) {
            *self.fail_flush_at.lock().unwrap() = Some(flush_count);
        }
    }

    impl LinuxImageDevice {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes: Mutex::new(bytes),
                flush_count: Mutex::new(0),
                fail_flush_at: Mutex::new(None),
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        fn flush_count(&self) -> usize {
            *self.flush_count.lock().unwrap()
        }

        fn fail_flush_at(&self, flush_count: usize) {
            *self.fail_flush_at.lock().unwrap() = Some(flush_count);
        }
    }

    impl Device for TestDevice {
        fn name(&self) -> &str {
            "kext4-mount-allocator-test"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Block
        }
    }

    impl BlockDevice for TestDevice {
        fn num_blocks(&self) -> u64 {
            (self.bytes.lock().unwrap().len() / TEST_BLOCK_SIZE) as u64
        }

        fn block_size(&self) -> usize {
            TEST_BLOCK_SIZE
        }

        fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
            let start = block_start(block_id)?;
            let end = start
                .checked_add(output.len())
                .ok_or(DriverError::InvalidInput)?;
            output.copy_from_slice(
                self.bytes
                    .lock()
                    .unwrap()
                    .get(start..end)
                    .ok_or(DriverError::InvalidInput)?,
            );
            Ok(())
        }

        fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
            let start = block_start(block_id)?;
            let end = start
                .checked_add(input.len())
                .ok_or(DriverError::InvalidInput)?;
            self.bytes
                .lock()
                .unwrap()
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?
                .copy_from_slice(input);
            Ok(())
        }

        fn flush(&self) -> DriverResult {
            let mut flush_count = self.flush_count.lock().unwrap();
            *flush_count += 1;
            if *self.fail_flush_at.lock().unwrap() == Some(*flush_count) {
                return Err(DriverError::Io);
            }
            Ok(())
        }
    }

    impl Device for LinuxImageDevice {
        fn name(&self) -> &str {
            "kext4-linux-allocator-test-image"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Block
        }
    }

    impl BlockDevice for LinuxImageDevice {
        fn num_blocks(&self) -> u64 {
            (self.bytes.lock().unwrap().len() / LINUX_IMAGE_DEVICE_BLOCK_SIZE) as u64
        }

        fn block_size(&self) -> usize {
            LINUX_IMAGE_DEVICE_BLOCK_SIZE
        }

        fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
            let start = linux_image_device_block_start(block_id)?;
            let end = start
                .checked_add(output.len())
                .ok_or(DriverError::InvalidInput)?;
            output.copy_from_slice(
                self.bytes
                    .lock()
                    .unwrap()
                    .get(start..end)
                    .ok_or(DriverError::InvalidInput)?,
            );
            Ok(())
        }

        fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
            let start = linux_image_device_block_start(block_id)?;
            let end = start
                .checked_add(input.len())
                .ok_or(DriverError::InvalidInput)?;
            self.bytes
                .lock()
                .unwrap()
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?
                .copy_from_slice(input);
            Ok(())
        }

        fn flush(&self) -> DriverResult {
            let mut flush_count = self.flush_count.lock().unwrap();
            *flush_count += 1;
            if *self.fail_flush_at.lock().unwrap() == Some(*flush_count) {
                return Err(DriverError::Io);
            }
            Ok(())
        }
    }

    #[test]
    fn sync_filesystem_flushes_device_and_reclaims_clean_metadata() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let metadata = filesystem
            .read_metadata_block(FilesystemBlock::new(2))
            .expect("read metadata through cache");
        drop(metadata);

        filesystem.sync_filesystem().expect("sync filesystem");

        assert_eq!(device.flush_count(), 1);
        assert_eq!(filesystem.metadata_io.reclaim_unused(1), 0);
    }

    #[test]
    fn sync_filesystem_propagates_flush_error() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        device.fail_flush_at(1);

        assert_eq!(
            filesystem.sync_filesystem(),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert_eq!(device.flush_count(), 1);
    }

    #[test]
    fn sync_filesystem_drains_pending_metadata_checkpoint() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let image = temporary_image_path("syncfs-pending-checkpoint");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated journaled allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated journaled image");
        let journal = filesystem
            .metadata_journal()
            .expect("open metadata journal");
        assert!(filesystem.journal_supports_revoke());
        let same_journal = filesystem
            .metadata_journal()
            .expect("reopen metadata journal");
        assert!(Arc::ptr_eq(&journal, &same_journal));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);

        filesystem
            .allocate_block(Some(block_goal), &mut handle)
            .expect("allocate a block from journaled Linux image");
        drop(handle);
        filesystem
            .enqueue_metadata_checkpoint_for_test(transaction)
            .expect("enqueue checkpoint");

        assert_eq!(filesystem.pending_checkpoint_count(), 1);
        let flush_count = device.flush_count();

        filesystem
            .sync_filesystem()
            .expect("sync pending checkpoint");

        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        assert!(device.flush_count() > flush_count);
        let next_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let next_transaction = next_handle.id();
        assert_eq!(next_transaction.get(), transaction.get().wrapping_add(1));
        drop(next_handle);
        assert_eq!(
            journal.running_transaction().unwrap(),
            Some(next_transaction)
        );
        fs::remove_file(image).expect("remove syncfs-pending-checkpoint image");
    }

    #[test]
    fn successful_operations_share_running_transaction_until_sync() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("journal-running-transaction-batch");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated journaled allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated journaled image");
        let credits = JournalCredits::new(4);
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);

        let journal = filesystem
            .metadata_journal_for_mutation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .expect("prepare first metadata mutation");
        let mut first_handle = journal.begin(credits).unwrap();
        let transaction = first_handle.id();
        let first_allocation = filesystem
            .allocate_block(Some(block_goal), &mut first_handle)
            .expect("allocate first block");
        filesystem
            .complete_metadata_mutation(first_handle, Ok(()))
            .expect("finish first metadata mutation");
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));

        let second_journal = filesystem
            .metadata_journal_for_mutation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .expect("prepare second metadata mutation");
        let mut second_handle = second_journal.begin(credits).unwrap();
        assert_eq!(second_handle.id(), transaction);
        let second_allocation = filesystem
            .allocate_block(Some(block_goal), &mut second_handle)
            .expect("allocate second block");
        filesystem
            .complete_metadata_mutation(second_handle, Ok(()))
            .expect("finish second metadata mutation");

        assert_ne!(first_allocation.block(), second_allocation.block());
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        filesystem
            .sync_filesystem()
            .expect("commit and checkpoint running transaction");
        assert_eq!(journal.running_transaction().unwrap(), None);
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        assert!(
            !filesystem
                .journal_status()
                .expect("clean journal status")
                .has_nonzero_log_start()
        );

        let cleanup_credits = JournalCredits::new(8);
        let cleanup_journal = filesystem
            .metadata_journal_for_mutation(
                cleanup_credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .expect("prepare cleanup metadata mutation");
        let mut cleanup_handle = cleanup_journal.begin(cleanup_credits).unwrap();
        filesystem
            .release_allocated_block(first_allocation.block(), &mut cleanup_handle)
            .expect("release first block");
        filesystem
            .release_allocated_block(second_allocation.block(), &mut cleanup_handle)
            .expect("release second block");
        filesystem
            .complete_metadata_mutation(cleanup_handle, Ok(()))
            .expect("finish cleanup metadata mutation");
        filesystem.sync_filesystem().expect("sync cleanup");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write batched journal image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove batched journal image");
    }

    #[test]
    fn inode_sync_conservatively_commits_the_current_running_transaction() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("inode-conservative-journal-sync");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated namespace image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated namespace image");
        let root = filesystem.root_inode().expect("read root inode");
        let first = filesystem
            .create_regular_file(
                &root,
                b"first.txt",
                0o644,
                1000,
                1000,
                crate::Ext4Timestamp::new(1, 0),
            )
            .expect("create first file");
        let journal = filesystem
            .metadata_journal()
            .expect("open metadata journal");
        let first_transaction = journal
            .running_transaction()
            .unwrap()
            .expect("first transaction remains running");

        let flushes_before_first_sync = device.flush_count();
        filesystem
            .sync_inode(first.child(), Ext4SyncIntent::FullMetadata)
            .expect("sync first inode transaction");
        // Persist ext4 recovery evidence, activate the clean journal, and
        // finish the log commit. No fourth sync_inode-only flush is needed.
        assert_eq!(device.flush_count(), flushes_before_first_sync + 3);

        assert_eq!(journal.running_transaction().unwrap(), None);
        assert_eq!(filesystem.pending_checkpoint_count(), 1);

        let flushes_before_data_only_sync = device.flush_count();
        filesystem
            .sync_inode(first.child(), Ext4SyncIntent::DataOnly)
            .expect("sync inode without a running metadata transaction");
        assert_eq!(device.flush_count(), flushes_before_data_only_sync + 1);

        let root = filesystem.root_inode().expect("reload root inode");
        let _second = filesystem
            .create_regular_file(
                &root,
                b"second.txt",
                0o644,
                1000,
                1000,
                crate::Ext4Timestamp::new(2, 0),
            )
            .expect("create second file");
        let second_transaction = journal
            .running_transaction()
            .unwrap()
            .expect("second transaction remains running");
        assert_ne!(second_transaction, first_transaction);

        let flushes_before_second_sync = device.flush_count();
        filesystem
            .sync_inode(first.child(), Ext4SyncIntent::FullMetadata)
            .expect("conservatively sync current transaction");
        // The next metadata mutation refreshes recovery evidence before its
        // commit. The commit barrier again replaces a trailing inode flush.
        assert_eq!(device.flush_count(), flushes_before_second_sync + 2);

        assert_eq!(journal.running_transaction().unwrap(), None);
        assert_eq!(filesystem.pending_checkpoint_count(), 2);

        filesystem.sync_filesystem().expect("sync remaining work");
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write conservative sync image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove conservative sync image");
    }

    #[test]
    fn journal_queue_appends_two_commits_and_advances_tail() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("journal-two-pending-commits");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated journaled allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated journaled image");
        let journal = filesystem
            .metadata_journal()
            .expect("open metadata journal");
        let free_blocks_before = filesystem.superblock().free_blocks_count();
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);

        let mut first_handle = journal.begin(JournalCredits::new(4)).unwrap();
        let first_transaction = first_handle.id();
        let first_allocation = filesystem
            .allocate_block(Some(block_goal), &mut first_handle)
            .expect("allocate first journaled block");
        drop(first_handle);
        filesystem
            .commit_metadata_transaction(first_transaction)
            .expect("commit first journal transaction");
        assert_eq!(filesystem.pending_checkpoint_count(), 1);

        let second_journal = filesystem
            .metadata_journal()
            .expect("join coordinator with pending checkpoint");
        assert!(Arc::ptr_eq(&journal, &second_journal));
        assert_eq!(filesystem.pending_checkpoint_count(), 1);
        let mut second_handle = second_journal.begin(JournalCredits::new(4)).unwrap();
        let second_transaction = second_handle.id();
        let second_allocation = filesystem
            .allocate_block(Some(block_goal), &mut second_handle)
            .expect("allocate second journaled block");
        drop(second_handle);
        filesystem
            .commit_metadata_transaction(second_transaction)
            .expect("commit second journal transaction");

        assert_ne!(first_allocation.block(), second_allocation.block());
        assert_eq!(filesystem.pending_checkpoint_count(), 2);
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_blocks_before - 2
        );
        assert!(filesystem.superblock().features().needs_recovery());
        let status = filesystem.journal_status().expect("journal status");
        assert_eq!(status.sequence(), first_transaction.get());
        assert!(status.has_nonzero_log_start());
        let scan = filesystem
            .scan_internal_journal()
            .expect("scan two commits");
        assert_eq!(scan.transactions().len(), 2);

        filesystem
            .run_checkpoint_worker_for_test()
            .expect("checkpoint first transaction");
        assert_eq!(filesystem.pending_checkpoint_count(), 1);
        assert!(filesystem.superblock().features().needs_recovery());
        let on_disk_bytes = device.bytes();
        let on_disk_superblock =
            Superblock::decode(&on_disk_bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
                .expect("decode checkpointed primary superblock");
        assert!(on_disk_superblock.features().needs_recovery());
        let status = filesystem.journal_status().expect("journal status");
        assert_eq!(status.sequence(), second_transaction.get());
        assert!(status.has_nonzero_log_start());
        let scan = filesystem
            .scan_internal_journal()
            .expect("scan remaining commit");
        assert_eq!(scan.transactions().len(), 1);
        assert_eq!(scan.transactions()[0].sequence(), second_transaction);

        filesystem
            .sync_filesystem()
            .expect("checkpoint remaining transaction");
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        assert!(!filesystem.superblock().features().needs_recovery());
        assert!(
            !filesystem
                .journal_status()
                .expect("clean journal status")
                .has_nonzero_log_start()
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_blocks_before - 2
        );

        let mut cleanup_handle = journal.begin(JournalCredits::new(8)).unwrap();
        let cleanup_transaction = cleanup_handle.id();
        filesystem
            .release_allocated_block(first_allocation.block(), &mut cleanup_handle)
            .expect("release first test block");
        filesystem
            .release_allocated_block(second_allocation.block(), &mut cleanup_handle)
            .expect("release second test block");
        drop(cleanup_handle);
        filesystem
            .commit_metadata_transaction(cleanup_transaction)
            .expect("commit cleanup transaction");
        assert_eq!(filesystem.pending_checkpoint_count(), 1);
        filesystem
            .sync_filesystem()
            .expect("checkpoint cleanup transaction");
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_blocks_before
        );
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write two-commit journal image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove two-commit journal image");
    }

    #[test]
    fn failed_checkpoint_worker_keeps_pending_work() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let image = temporary_image_path("syncfs-pending-checkpoint-failure");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated journaled allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated journaled image");
        let journal = filesystem
            .metadata_journal()
            .expect("open metadata journal");
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);

        filesystem
            .allocate_block(Some(block_goal), &mut handle)
            .expect("allocate a block from journaled Linux image");
        drop(handle);
        filesystem
            .enqueue_metadata_checkpoint_for_test(transaction)
            .expect("enqueue checkpoint");
        device.fail_flush_at(device.flush_count() + 1);

        assert_eq!(
            filesystem.sync_filesystem(),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert_eq!(filesystem.pending_checkpoint_count(), 1);
        assert!(journal.is_aborted());
        assert_eq!(filesystem.sync_filesystem(), Err(Ext4Error::JournalAborted));
        fs::remove_file(image).expect("remove syncfs-pending-checkpoint-failure image");
    }

    #[test]
    fn e2fsck_accepts_allocator_round_trip_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("allocator-round-trip");
        create_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated allocator image");
        let journal = JournalTransactions::new(TransactionId::new(701));
        let mut handle = journal.begin(JournalCredits::new(14)).unwrap();
        let transaction = handle.id();
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);
        let parent_inode = InodeNumber::new(filesystem.superblock().inodes_per_group() + 1);

        let block = filesystem
            .allocate_block(Some(block_goal), &mut handle)
            .expect("allocate a data block from Linux image");
        filesystem
            .release_allocated_block(block.block(), &mut handle)
            .expect("release allocated data block");
        let inode = filesystem
            .allocate_inode(
                Some(parent_inode),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .expect("allocate an inode from Linux image");
        filesystem
            .release_allocated_inode(inode.inode(), InodeKind::RegularFile, &mut handle)
            .expect("release allocated inode");
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write allocator-round-trip image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove allocator-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_truncate_round_trip_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let debugfs = require_e2fsprogs("debugfs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("truncate-round-trip");
        let host_file = temporary_image_path("truncate-round-trip-host");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let input = vec![0x7b; TEST_BLOCK_SIZE * 3];
        fs::write(&host_file, &input).expect("write truncate host file");
        run_debugfs(
            &debugfs,
            &image,
            &format!("write {} /truncate.bin", host_file.display()),
        );
        fs::remove_file(&host_file).expect("remove truncate host file");

        let bytes = fs::read(&image).expect("read generated truncate image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated truncate image");
        let root = filesystem.root_inode().expect("read root inode");
        let entry = filesystem
            .lookup(&root, "truncate.bin")
            .expect("lookup truncate file")
            .expect("truncate file exists");
        let inode = filesystem
            .inode(entry.inode())
            .expect("read truncate inode");
        let free_before_truncate = filesystem.superblock().free_blocks_count();
        let new_size = u64::try_from(TEST_BLOCK_SIZE + 23).unwrap();

        let truncated = filesystem
            .truncate_regular_inode(&inode, new_size, crate::Ext4Timestamp::new(44, 0))
            .expect("truncate Linux-created file");

        assert_eq!(truncated.size(), new_size);
        assert_eq!(truncated.blocks(), 16);
        assert_eq!(filesystem.orphan_head(), None);
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_truncate + 1
        );
        let crate::BlockMapping::Mapped { physical, .. } = filesystem
            .map_blocks(&truncated, LogicalBlock::new(1))
            .expect("map partial EOF block")
        else {
            panic!("partial EOF block should remain mapped");
        };
        let mut eof_block = vec![0xff; TEST_BLOCK_SIZE];
        filesystem
            .read_blocks(FilesystemBlock::new(physical.get()), 1, &mut eof_block)
            .expect("read partial EOF block");
        assert_eq!(&eof_block[..23], &[0x7b; 23]);
        assert!(eof_block[23..].iter().all(|byte| *byte == 0));
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write truncate-round-trip image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove truncate-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_regular_file_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let debugfs = require_e2fsprogs("debugfs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-create-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated namespace image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated namespace image");
        let root = filesystem.root_inode().expect("read root inode");
        assert_eq!(filesystem.lookup(&root, "kext4-created.txt").unwrap(), None);

        let created = filesystem
            .create_regular_file(
                &root,
                b"kext4-created.txt",
                0o644,
                1000,
                1001,
                crate::Ext4Timestamp::new(123, 0),
            )
            .expect("create regular file in linear root directory");

        assert_eq!(created.child().kind(), InodeKind::RegularFile);
        assert_eq!(created.child().links_count(), 1);
        assert_eq!(created.child().size(), 0);
        assert_eq!(created.child().uid(), 1000);
        assert_eq!(created.child().gid(), 1001);
        let entry = filesystem
            .lookup(created.parent(), "kext4-created.txt")
            .expect("lookup created file")
            .expect("created file is visible");
        assert_eq!(entry.inode(), created.child().number());
        assert_eq!(entry.file_type(), crate::DirectoryFileType::RegularFile);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write namespace image");
        run_debugfs(&debugfs, &image, "stat /kext4-created.txt");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-create-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_directory_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let debugfs = require_e2fsprogs("debugfs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-mkdir-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated mkdir image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount mkdir image");
        let root = filesystem.root_inode().expect("read root inode");
        let old_root_links = root.links_count();

        let created = filesystem
            .create_directory(
                &root,
                b"kext4-dir",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(124, 0),
            )
            .expect("create linear directory");

        assert_eq!(created.child().kind(), InodeKind::Directory);
        assert_eq!(created.child().links_count(), 2);
        assert_eq!(created.parent().links_count(), old_root_links + 1);
        let dot = filesystem
            .lookup(created.child(), ".")
            .expect("lookup dot")
            .expect("dot exists");
        let dotdot = filesystem
            .lookup(created.child(), "..")
            .expect("lookup dotdot")
            .expect("dotdot exists");
        assert_eq!(dot.inode(), created.child().number());
        assert_eq!(dotdot.inode(), created.parent().number());
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write mkdir image");
        run_debugfs(&debugfs, &image, "stat /kext4-dir");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-mkdir-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_regular_file_unlink_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-unlink-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated unlink image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount unlink image");
        let root = filesystem.root_inode().expect("read root inode");
        let created = filesystem
            .create_regular_file(
                &root,
                b"kext4-unlink.txt",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(125, 0),
            )
            .expect("create unlink target");
        let written = filesystem
            .writeback_ordered_at(
                created.child(),
                0,
                b"kext4 unlink data",
                17,
                crate::Ext4Timestamp::new(126, 0),
                crate::Ext4SyncIntent::FullMetadata,
            )
            .expect("write data before unlink");
        assert!(written.blocks() > 0);

        let removed = filesystem
            .unlink(
                created.parent(),
                b"kext4-unlink.txt",
                crate::Ext4Timestamp::new(127, 0),
            )
            .expect("unlink regular file");
        assert_eq!(removed.removed_inode(), written.number());
        assert_eq!(removed.removed().links_count(), 0);
        assert_eq!(
            filesystem
                .lookup(removed.parent(), "kext4-unlink.txt")
                .expect("lookup removed file"),
            None
        );
        let held = filesystem
            .referenced_inode(removed.removed_inode())
            .expect("open reference can reload a zero-link inode");
        let mut original = [0; 17];
        assert_eq!(
            filesystem.read_at(&held, 0, &mut original).unwrap(),
            original.len()
        );
        assert_eq!(&original, b"kext4 unlink data");
        let held = filesystem
            .writeback_ordered_at(
                &held,
                17,
                b" remains open",
                30,
                crate::Ext4Timestamp::new(128, 0),
                crate::Ext4SyncIntent::FullMetadata,
            )
            .expect("write through open reference after unlink");
        assert_eq!(held.links_count(), 0);
        let mut open_data = [0; 30];
        assert_eq!(
            filesystem.read_at(&held, 0, &mut open_data).unwrap(),
            open_data.len()
        );
        assert_eq!(&open_data, b"kext4 unlink data remains open");
        filesystem
            .evict_unlinked_inode(removed.removed_inode(), crate::Ext4Timestamp::new(129, 0))
            .expect("evict unlinked regular file after final reference");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write unlink image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-unlink-round-trip image");
    }

    #[test]
    fn recovery_evicts_clean_zero_link_orphan_on_huge_file_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("clean-zero-link-orphan-recovery");
        create_journaled_huge_file_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated orphan recovery image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount orphan recovery image");
        let root = filesystem.root_inode().expect("read root inode");
        let created = filesystem
            .create_regular_file(
                &root,
                b"kext4-recovery-orphan",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(130, 0),
            )
            .expect("create orphan recovery target");
        let written = filesystem
            .writeback_ordered_at(
                created.child(),
                0,
                b"recover zero-link inode",
                23,
                crate::Ext4Timestamp::new(131, 0),
                crate::Ext4SyncIntent::FullMetadata,
            )
            .expect("write ordinary inode on huge_file filesystem");
        let removed = filesystem
            .unlink(
                created.parent(),
                b"kext4-recovery-orphan",
                crate::Ext4Timestamp::new(132, 0),
            )
            .expect("leave zero-link inode on the legacy orphan list");
        assert_eq!(removed.removed_inode(), written.number());
        assert_eq!(removed.removed().links_count(), 0);
        assert_eq!(filesystem.orphan_head(), Some(written.number()));
        assert!(!filesystem.superblock().features().needs_recovery());
        drop(filesystem);

        let mount_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::mount(mount_device).map(|_| ()),
            Err(Ext4Error::NeedsRecovery)
        );
        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(Ext4Filesystem::recover(recovery_device), Ok(None));

        let recovered_device: Arc<dyn BlockDevice> = device.clone();
        let recovered =
            Ext4Filesystem::mount(recovered_device).expect("mount orphan-cleaned image");
        assert_eq!(recovered.orphan_head(), None);
        assert_eq!(recovered.inode(written.number()), Err(Ext4Error::NotFound));
        assert_eq!(
            recovered
                .lookup(&recovered.root_inode().unwrap(), "kext4-recovery-orphan")
                .expect("lookup recovered namespace"),
            None
        );
        drop(recovered);

        fs::write(&image, device.bytes()).expect("write recovered orphan image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove clean-zero-link-orphan-recovery image");
    }

    #[test]
    fn e2fsck_accepts_linear_directory_remove_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-rmdir-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated rmdir image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount rmdir image");
        let root = filesystem.root_inode().expect("read root inode");
        let created = filesystem
            .create_directory(
                &root,
                b"kext4-rmdir",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(128, 0),
            )
            .expect("create rmdir target");

        let removed = filesystem
            .remove_directory(
                created.parent(),
                b"kext4-rmdir",
                crate::Ext4Timestamp::new(129, 0),
            )
            .expect("remove empty directory");
        assert_eq!(removed.removed_inode(), created.child().number());
        assert_eq!(removed.removed().links_count(), 0);
        assert_eq!(removed.parent().links_count(), root.links_count());
        assert_eq!(
            filesystem
                .lookup(removed.parent(), "kext4-rmdir")
                .expect("lookup removed directory"),
            None
        );
        filesystem
            .evict_unlinked_inode(removed.removed_inode(), crate::Ext4Timestamp::new(130, 0))
            .expect("evict removed directory after final reference");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write rmdir image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-rmdir-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_hard_link_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-link-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated link image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount link image");
        let root = filesystem.root_inode().expect("read root inode");
        let created = filesystem
            .create_regular_file(
                &root,
                b"kext4-link-src.txt",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(130, 0),
            )
            .expect("create hard link source");
        let linked = filesystem
            .link(
                created.parent(),
                b"kext4-link-dst.txt",
                created.child(),
                crate::Ext4Timestamp::new(131, 0),
            )
            .expect("create hard link");

        assert_eq!(linked.target().number(), created.child().number());
        assert_eq!(linked.target().links_count(), 2);
        let source = filesystem
            .lookup(linked.parent(), "kext4-link-src.txt")
            .expect("lookup source")
            .expect("source remains visible");
        let target = filesystem
            .lookup(linked.parent(), "kext4-link-dst.txt")
            .expect("lookup hard link")
            .expect("hard link is visible");
        assert_eq!(source.inode(), target.inode());

        let removed = filesystem
            .unlink(
                linked.parent(),
                b"kext4-link-src.txt",
                crate::Ext4Timestamp::new(132, 0),
            )
            .expect("unlink one hard-link name");
        let remaining = filesystem
            .lookup(removed.parent(), "kext4-link-dst.txt")
            .expect("lookup remaining hard link")
            .expect("remaining hard link is visible");
        let remaining_inode = filesystem
            .inode(remaining.inode())
            .expect("read remaining hard-link inode");
        assert_eq!(remaining_inode.links_count(), 1);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write link image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-link-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_regular_file_rename_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-rename-file-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated file rename image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount rename image");
        let root = filesystem.root_inode().expect("read root inode");
        let left = filesystem
            .create_directory(
                &root,
                b"left",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(133, 0),
            )
            .expect("create left directory");
        let root = left.parent().clone();
        let right = filesystem
            .create_directory(
                &root,
                b"right",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(134, 0),
            )
            .expect("create right directory");
        let left_file = filesystem
            .create_regular_file(
                left.child(),
                b"move-me.txt",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(135, 0),
            )
            .expect("create file to rename");

        let renamed = filesystem
            .rename(
                left_file.parent(),
                b"move-me.txt",
                right.child(),
                b"moved.txt",
                crate::Ext4Timestamp::new(136, 0),
            )
            .expect("rename file across directories");
        assert_eq!(
            filesystem
                .lookup(renamed.source_parent(), "move-me.txt")
                .expect("lookup old file name"),
            None
        );
        let moved = filesystem
            .lookup(renamed.target_parent(), "moved.txt")
            .expect("lookup moved file")
            .expect("moved file is visible");
        assert_eq!(moved.inode(), left_file.child().number());
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write file rename image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-rename-file-round-trip image");
    }

    #[test]
    fn rename_overwrite_defers_data_victim_eviction_credits() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-rename-file-overwrite-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated file overwrite rename image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount overwrite rename image");
        let root = filesystem.root_inode().expect("read root inode");
        let source = filesystem
            .create_regular_file(
                &root,
                b"rename-src.txt",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(145, 0),
            )
            .expect("create rename source");
        let target = filesystem
            .create_regular_file(
                source.parent(),
                b"rename-dst.txt",
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(146, 0),
            )
            .expect("create rename target");
        let target_with_data = filesystem
            .writeback_ordered_at(
                target.child(),
                0,
                b"rename victim data",
                18,
                crate::Ext4Timestamp::new(147, 0),
                crate::Ext4SyncIntent::FullMetadata,
            )
            .expect("write data to rename target");
        assert!(target_with_data.blocks() > 0);
        // Namespace replacement only records the victim as a zero-link
        // orphan. Its extent cleanup belongs to final eviction below.
        let renamed = filesystem
            .rename(
                target.parent(),
                b"rename-src.txt",
                target.parent(),
                b"rename-dst.txt",
                crate::Ext4Timestamp::new(148, 0),
            )
            .expect("rename file over existing file");
        assert_eq!(
            filesystem
                .lookup(renamed.source_parent(), "rename-src.txt")
                .expect("lookup overwritten source name"),
            None
        );
        let moved = filesystem
            .lookup(renamed.target_parent(), "rename-dst.txt")
            .expect("lookup overwritten target name")
            .expect("target name is still visible");
        assert_eq!(moved.inode(), source.child().number());
        let replaced = renamed
            .replaced()
            .expect("rename reports overwritten inode");
        assert_eq!(replaced.number(), target_with_data.number());
        assert_eq!(replaced.links_count(), 0);
        filesystem
            .evict_unlinked_inode(replaced.number(), crate::Ext4Timestamp::new(149, 0))
            .expect("evict overwritten regular file after final reference");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write overwrite file rename image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-rename-file-overwrite-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_directory_rename_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-rename-dir-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated directory rename image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount directory rename image");
        let root = filesystem.root_inode().expect("read root inode");
        let old_root_links = root.links_count();
        let left = filesystem
            .create_directory(
                &root,
                b"left",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(137, 0),
            )
            .expect("create left directory");
        let root = left.parent().clone();
        let right = filesystem
            .create_directory(
                &root,
                b"right",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(138, 0),
            )
            .expect("create right directory");
        let child = filesystem
            .create_directory(
                left.child(),
                b"child",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(139, 0),
            )
            .expect("create child directory to rename");

        let renamed = filesystem
            .rename(
                child.parent(),
                b"child",
                right.child(),
                b"child-renamed",
                crate::Ext4Timestamp::new(140, 0),
            )
            .expect("rename directory across parents");
        let moved = filesystem
            .lookup(renamed.target_parent(), "child-renamed")
            .expect("lookup moved directory")
            .expect("moved directory is visible");
        assert_eq!(moved.inode(), child.child().number());
        let moved_inode = filesystem
            .inode(moved.inode())
            .expect("read moved directory");
        let dotdot = filesystem
            .lookup(&moved_inode, "..")
            .expect("lookup moved dotdot")
            .expect("moved dotdot exists");
        assert_eq!(dotdot.inode(), right.child().number());
        let root_after = filesystem.root_inode().expect("read updated root");
        assert_eq!(root_after.links_count(), old_root_links + 2);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write directory rename image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-rename-dir-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_directory_rename_overwrite_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-rename-dir-overwrite-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated directory overwrite rename image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount directory overwrite rename image");
        let root = filesystem.root_inode().expect("read root inode");
        let source = filesystem
            .create_directory(
                &root,
                b"rename-src-dir",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(148, 0),
            )
            .expect("create rename source directory");
        let target = filesystem
            .create_directory(
                source.parent(),
                b"rename-dst-dir",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(149, 0),
            )
            .expect("create rename target directory");
        let parent_links_before = target.parent().links_count();
        let renamed = filesystem
            .rename(
                target.parent(),
                b"rename-src-dir",
                target.parent(),
                b"rename-dst-dir",
                crate::Ext4Timestamp::new(150, 0),
            )
            .expect("rename directory over existing empty directory");
        assert_eq!(
            renamed.target_parent().links_count(),
            parent_links_before - 1
        );
        assert_eq!(
            filesystem
                .lookup(renamed.source_parent(), "rename-src-dir")
                .expect("lookup overwritten source directory name"),
            None
        );
        let moved = filesystem
            .lookup(renamed.target_parent(), "rename-dst-dir")
            .expect("lookup overwritten target directory name")
            .expect("target directory name is visible");
        assert_eq!(moved.inode(), source.child().number());
        let replaced = renamed
            .replaced()
            .expect("rename reports overwritten directory");
        assert_eq!(replaced.number(), target.child().number());
        assert_eq!(replaced.links_count(), 0);
        filesystem
            .evict_unlinked_inode(replaced.number(), crate::Ext4Timestamp::new(151, 0))
            .expect("evict overwritten directory after final reference");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write overwrite directory rename image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-rename-dir-overwrite-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_fast_symlink_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-fast-symlink-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated symlink image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount symlink image");
        let root = filesystem.root_inode().expect("read root inode");
        let created = filesystem
            .create_fast_symlink(
                &root,
                b"kext4-symlink",
                b"target/path",
                0,
                0,
                crate::Ext4Timestamp::new(141, 0),
            )
            .expect("create fast symlink");
        assert_eq!(created.child().kind(), InodeKind::Symlink);
        assert_eq!(created.child().size(), 11);
        let entry = filesystem
            .lookup(created.parent(), "kext4-symlink")
            .expect("lookup symlink")
            .expect("symlink is visible");
        assert_eq!(entry.file_type(), crate::DirectoryFileType::Symlink);
        let mut target = [0; 16];
        let read = filesystem
            .read_link_at(created.child(), 0, &mut target)
            .expect("read fast symlink");
        assert_eq!(&target[..read], b"target/path");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write symlink image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-fast-symlink-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_block_mapped_symlink_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-block-symlink-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated block symlink image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount symlink image");
        let root = filesystem.root_inode().expect("read root inode");
        let target = vec![b'a'; 128];
        let created = filesystem
            .create_symlink(
                &root,
                b"kext4-block-symlink",
                &target,
                0,
                0,
                crate::Ext4Timestamp::new(144, 0),
            )
            .expect("create block-mapped symlink");
        assert_eq!(created.child().kind(), InodeKind::Symlink);
        assert_eq!(created.child().size(), 128);
        assert_ne!(created.child().blocks(), 0);
        assert!(created.child().has_extents());

        let entry = filesystem
            .lookup(created.parent(), "kext4-block-symlink")
            .expect("lookup block symlink")
            .expect("block symlink is visible");
        assert_eq!(entry.file_type(), crate::DirectoryFileType::Symlink);
        let mut read_target = vec![0; target.len()];
        let read = filesystem
            .read_link_at(created.child(), 0, &mut read_target)
            .expect("read block-mapped symlink");
        assert_eq!(read, target.len());
        assert_eq!(read_target, target);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write block symlink image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-block-symlink-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_special_file_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-special-round-trip");
        create_journaled_linear_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated special file image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem = Ext4Filesystem::mount(block_device).expect("mount special file image");
        let root = filesystem.root_inode().expect("read root inode");
        let fifo = filesystem
            .create_special_file(
                &root,
                b"kext4-fifo",
                (InodeKind::Fifo, None),
                0o644,
                0,
                0,
                crate::Ext4Timestamp::new(142, 0),
            )
            .expect("create fifo");
        let char_device = filesystem
            .create_special_file(
                fifo.parent(),
                b"kext4-null",
                (
                    InodeKind::CharacterDevice,
                    Some(crate::Ext4DeviceId::new(1, 3)),
                ),
                0o666,
                0,
                0,
                crate::Ext4Timestamp::new(143, 0),
            )
            .expect("create char device");
        assert_eq!(fifo.child().kind(), InodeKind::Fifo);
        assert_eq!(char_device.child().kind(), InodeKind::CharacterDevice);
        assert_eq!(
            char_device
                .child()
                .device_id()
                .expect("decode char device id"),
            Some(crate::Ext4DeviceId::new(1, 3))
        );
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write special file image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-special-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_indexed_directory_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let debugfs = require_e2fsprogs("debugfs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-indexed-create-round-trip");
        create_journaled_indexed_namespace_test_image(&mke2fs, &image);
        run_debugfs(&debugfs, &image, "mkdir /big");
        for index in 0..1000 {
            run_debugfs(
                &debugfs,
                &image,
                &format!("write /dev/null /big/f{index:04}"),
            );
        }
        run_e2fsck_rebuild_index(&e2fsck, &image);

        let bytes = fs::read(&image).expect("read generated indexed image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount indexed namespace image");
        let root = filesystem.root_inode().expect("read root inode");
        let big_entry = filesystem
            .lookup(&root, "big")
            .expect("lookup indexed directory")
            .expect("indexed directory exists");
        let big = filesystem
            .inode(big_entry.inode())
            .expect("read indexed directory inode");
        assert!(big.has_indexed_directory());
        let mut parent = big;
        let mut last_name = Vec::new();
        let mut created = None;
        for index in 0..300 {
            last_name = format!("kext4-added-{index:04}").into_bytes();
            let next = filesystem
                .create_regular_file(
                    &parent,
                    &last_name,
                    0o644,
                    0,
                    0,
                    crate::Ext4Timestamp::new(144 + index, 0),
                )
                .expect("create regular file in indexed directory");
            parent = next.parent().clone();
            created = Some(next);
        }
        let renamed = filesystem
            .rename(
                &parent,
                b"f0000",
                &parent,
                b"kext4-renamed",
                crate::Ext4Timestamp::new(600, 0),
            )
            .expect("rename inside indexed directory");
        parent = renamed.target_parent().clone();
        let removed = filesystem
            .unlink(&parent, b"f0001", crate::Ext4Timestamp::new(601, 0))
            .expect("unlink inside indexed directory");
        parent = removed.parent().clone();
        assert!(created.is_some());
        let entry = filesystem
            .lookup_bytes(&parent, &last_name)
            .expect("lookup indexed create")
            .expect("indexed create is visible");
        assert_eq!(entry.file_type(), crate::DirectoryFileType::RegularFile);
        assert!(
            filesystem
                .lookup(&parent, "kext4-renamed")
                .expect("lookup indexed rename")
                .is_some()
        );
        assert!(
            filesystem
                .lookup(&parent, "f0001")
                .expect("lookup indexed unlink")
                .is_none()
        );
        filesystem
            .evict_unlinked_inode(removed.removed_inode(), crate::Ext4Timestamp::new(602, 0))
            .expect("evict unlinked indexed-directory child");
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write indexed image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-indexed-create-round-trip image");
    }

    #[test]
    fn e2fsck_accepts_linear_to_indexed_directory_create_on_linux_image() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let e2fsck = require_e2fsprogs("e2fsck");
        let image = temporary_image_path("namei-linear-to-indexed-round-trip");
        create_journaled_indexed_namespace_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated dir_index image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount dir_index namespace image");
        let root = filesystem.root_inode().expect("read root inode");
        let directory = filesystem
            .create_directory(
                &root,
                b"kext4-big",
                0o755,
                0,
                0,
                crate::Ext4Timestamp::new(500, 0),
            )
            .expect("create directory before htree conversion");
        let mut parent = directory.child().clone();
        let mut last_name = Vec::new();
        for index in 0..260 {
            last_name = format!("entry-{index:04}").into_bytes();
            let next = filesystem
                .create_regular_file(
                    &parent,
                    &last_name,
                    0o644,
                    0,
                    0,
                    crate::Ext4Timestamp::new(501 + index, 0),
                )
                .expect("create regular file during htree conversion");
            parent = next.parent().clone();
        }
        assert!(parent.has_indexed_directory());
        let entry = filesystem
            .lookup_bytes(&parent, &last_name)
            .expect("lookup converted indexed directory")
            .expect("converted indexed entry is visible");
        assert_eq!(entry.file_type(), crate::DirectoryFileType::RegularFile);
        drop(filesystem);

        fs::write(&image, device.bytes()).expect("write converted indexed image");
        run_e2fsck_read_only(&e2fsck, &image);
        fs::remove_file(image).expect("remove namei-linear-to-indexed-round-trip image");
    }

    #[test]
    fn recovers_persisted_allocator_journal_commit_before_checkpoint() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let image = temporary_image_path("allocator-journal-recover");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated journaled allocator image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated journaled image");
        let journal = filesystem
            .metadata_journal()
            .expect("journaled test image has an internal journal");
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        let block_goal =
            FilesystemBlock::new(u64::from(filesystem.superblock().blocks_per_group()) + 10);

        let allocation = filesystem
            .allocate_block(Some(block_goal), &mut handle)
            .expect("allocate a block from journaled Linux image");
        let group_index = usize::try_from(allocation.group().get()).unwrap();
        let expected_super_free = filesystem.superblock().free_blocks_count();
        let expected_group_free = filesystem.groups()[group_index].free_blocks_count();
        let bitmap_block = FilesystemBlock::new(filesystem.groups()[group_index].block_bitmap());
        let bitmap_bit = allocation.bitmap_bit();
        drop(handle);

        let commit = journal.force_commit_for_test(transaction).unwrap();
        let expected_replay_updates = commit.metadata_blocks().unwrap().as_ref().len();
        filesystem
            .persist_metadata_journal_commit(&commit)
            .expect("persist allocator metadata to the journal");
        drop(filesystem);

        let dirty_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::mount(dirty_device).map(|_| ()),
            Err(Ext4Error::NeedsRecovery)
        );
        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        let report = Ext4Filesystem::recover(recovery_device)
            .expect("recover persisted allocator journal commit")
            .expect("journal recovery was required");
        assert_eq!(report.update_count(), expected_replay_updates);

        let recovered_device: Arc<dyn BlockDevice> = device.clone();
        let recovered =
            Ext4Filesystem::mount(recovered_device).expect("mount recovered allocator image");
        assert!(!recovered.superblock().features().needs_recovery());
        assert_eq!(
            recovered.superblock().free_blocks_count(),
            expected_super_free
        );
        assert_eq!(
            recovered.groups()[group_index].free_blocks_count(),
            expected_group_free
        );
        let bitmap = recovered.read_metadata_block(bitmap_block).unwrap();
        let bitmap_byte = usize::try_from(bitmap_bit / 8).unwrap();
        let bitmap_mask = 1u8 << (bitmap_bit % 8);
        assert_ne!(bitmap.as_ref()[bitmap_byte] & bitmap_mask, 0);
        drop(recovered);

        fs::remove_file(image).expect("remove allocator-journal-recover image");
    }

    #[test]
    fn block_allocator_uses_goal_group_and_falls_back_across_groups() {
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: 0,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0xff; 4],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0, 0, 0, 0],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(801));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        let allocation = filesystem
            .allocate_block(Some(FilesystemBlock::new(8)), &mut handle)
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.block(), PhysicalBlock::new(38));
    }

    #[test]
    fn block_allocator_starts_scan_at_goal_inside_selected_group() {
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0, 0, 0, 0],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(811));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        let allocation = filesystem
            .allocate_block(Some(FilesystemBlock::new(40)), &mut handle)
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.block(), PhysicalBlock::new(40));
        assert_eq!(allocation.bitmap_bit(), 8);
    }

    #[test]
    fn inode_allocator_uses_parent_group_for_regular_inode_and_spreads_directories() {
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 7,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0, 0, 0, 0],
            },
        ]);

        let journal = JournalTransactions::new(TransactionId::new(821));
        let mut regular_handle = journal.begin(JournalCredits::new(4)).unwrap();
        let regular = filesystem
            .allocate_inode(
                Some(InodeNumber::new(40)),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut regular_handle,
            )
            .unwrap();
        assert_eq!(regular.group(), BlockGroupNumber::new(1));
        assert_eq!(regular.inode(), InodeNumber::new(33));
        drop(regular_handle);

        let commit = journal.force_commit(TransactionId::new(821)).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let mut directory_handle = journal.begin(JournalCredits::new(4)).unwrap();
        let directory = filesystem
            .allocate_inode(
                None,
                InodeInitialization::directory(0o755, 0, 0),
                &mut directory_handle,
            )
            .unwrap();
        assert_eq!(directory.group(), BlockGroupNumber::new(1));
        assert_eq!(filesystem.groups()[1].used_directories_count(), 1);
    }

    #[test]
    fn orlov_top_level_directories_spread_across_groups() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        }; 4];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        let journal = JournalTransactions::new(TransactionId::new(822));
        let mut handle = journal.begin(JournalCredits::new(32)).unwrap();
        let mut allocated_groups = Vec::new();

        for _ in 0..4 {
            let allocation = filesystem
                .allocate_inode(
                    None,
                    InodeInitialization::directory(0o755, 0, 0),
                    &mut handle,
                )
                .unwrap();
            allocated_groups.push(allocation.group().get());
        }

        allocated_groups.sort_unstable();
        allocated_groups.dedup();
        assert_eq!(allocated_groups, vec![0, 1, 2, 3]);
    }

    #[test]
    fn orlov_top_level_directory_uses_child_name_hash_start() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        }; 4];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        let child_name = b"hashed-top-level-dir";
        let expected_flex = filesystem.orlov_top_level_start_flex(Some(child_name));
        let journal = JournalTransactions::new(TransactionId::new(826));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

        let allocation = filesystem
            .allocate_named_inode(
                Some(InodeNumber::new(2)),
                child_name,
                InodeInitialization::directory(0o755, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(expected_flex));
    }

    #[test]
    fn orlov_directory_group_requires_free_blocks_inside_flex() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
            inode_bitmap: [0, 0, 0, 0],
        }; 2];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        groups[1] = AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 7,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        };
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        enable_allocator_flex_bg(&mut filesystem, 1);
        let journal = JournalTransactions::new(TransactionId::new(825));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

        let allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::directory(0o755, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    }

    #[test]
    fn regular_inode_allocator_uses_quadratic_probe_before_linear_fallback() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: 0,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
            inode_bitmap: [0xff; 4],
        }; 8];
        groups[0] = AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
            inode_bitmap: [0xff, 0x03, 0, 0],
        };
        groups[3] = AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        };
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        let journal = JournalTransactions::new(TransactionId::new(823));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

        let allocation = filesystem
            .allocate_inode(
                Some(InodeNumber::new(11)),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(3));
    }

    #[test]
    fn regular_inode_allocator_keeps_parent_flex_group_locality() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        }; 4];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        groups[2] = AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
            inode_bitmap: [0, 0, 0, 0],
        };
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        enable_allocator_flex_bg(&mut filesystem, 1);
        let journal = JournalTransactions::new(TransactionId::new(824));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

        let allocation = filesystem
            .allocate_inode(
                Some(InodeNumber::new(65)),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(3));
    }

    #[test]
    fn block_allocator_journals_bitmap_group_and_superblock_updates() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let available_before = filesystem.blocks_available_for_reservation();
        let journal = JournalTransactions::new(TransactionId::new(101));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();

        let allocation = filesystem
            .allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle)
            .unwrap();

        assert_eq!(allocation.block(), PhysicalBlock::new(6));
        assert_eq!(allocation.bitmap_bit(), 6);
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 25);
        assert_eq!(filesystem.superblock().free_blocks_count(), 25);
        assert_eq!(
            filesystem.blocks_available_for_reservation(),
            available_before - 1
        );
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 3);
        assert_eq!(
            commit.metadata_blocks().unwrap().as_ref(),
            &[
                FilesystemBlock::new(0),
                FilesystemBlock::new(1),
                FilesystemBlock::new(2),
            ]
        );

        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0b0100_0000);
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 25);
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 25);
    }

    #[test]
    fn mballoc_allocates_goal_aligned_contiguous_run() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(111));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        let request = Ext4AllocationRequest::new(
            LogicalBlock::new(7),
            Some(FilesystemBlock::new(8)),
            BlockCount::new(4),
            BlockCount::new(4),
            Ext4AllocationFlags::EXACT,
            BlockGroupNumber::new(0),
        )
        .unwrap();

        let allocation = filesystem
            .allocate_blocks_for_write(request, &mut handle)
            .unwrap();

        assert_eq!(allocation.logical_start(), LogicalBlock::new(7));
        assert_eq!(allocation.group(), BlockGroupNumber::new(0));
        assert_eq!(allocation.physical_start(), PhysicalBlock::new(8));
        assert_eq!(allocation.block_count(), BlockCount::new(4));
        assert_eq!(allocation.requested_len(), BlockCount::new(4));
        assert!(!allocation.is_partial());
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 22);
        assert_eq!(filesystem.superblock().free_blocks_count(), 22);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 3);
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[2 * TEST_BLOCK_SIZE + 1] & 0b0000_1111, 0b0000_1111);
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 22);
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 22);
    }

    #[test]
    fn mballoc_returns_explicit_partial_run_when_fragmented() {
        let (mut filesystem, _device) =
            allocator_multigroup_test_filesystem(&[AllocatorGroupSpec {
                free_blocks: 2,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0xff, 0b1111_1100, 0xff, 0xff],
                inode_bitmap: [0xff, 0x03, 0, 0],
            }]);
        let journal = JournalTransactions::new(TransactionId::new(112));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let request = Ext4AllocationRequest::new(
            LogicalBlock::new(11),
            Some(FilesystemBlock::new(8)),
            BlockCount::new(4),
            BlockCount::new(2),
            Ext4AllocationFlags::ALLOW_PARTIAL,
            BlockGroupNumber::new(0),
        )
        .unwrap();

        let allocation = filesystem
            .allocate_blocks_for_write(request, &mut handle)
            .unwrap();

        assert_eq!(allocation.physical_start(), PhysicalBlock::new(8));
        assert_eq!(allocation.block_count(), BlockCount::new(2));
        assert_eq!(allocation.requested_len(), BlockCount::new(4));
        assert!(allocation.is_partial());
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 0);
        assert_eq!(filesystem.superblock().free_blocks_count(), 0);
    }

    #[test]
    fn mballoc_falls_back_when_group_cannot_satisfy_minimum_run() {
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: 1,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b1011_1111, 0xff, 0xff, 0xff],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0, 0, 0, 0],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(113));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let request = Ext4AllocationRequest::new(
            LogicalBlock::new(12),
            None,
            BlockCount::new(2),
            BlockCount::new(2),
            Ext4AllocationFlags::EXACT,
            BlockGroupNumber::new(0),
        )
        .unwrap();

        let allocation = filesystem
            .allocate_blocks_for_write(request, &mut handle)
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.physical_start(), PhysicalBlock::new(38));
        assert_eq!(allocation.block_count(), BlockCount::new(2));
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 1);
        assert_eq!(filesystem.groups()[1].free_blocks_count(), 24);
    }

    #[test]
    fn mballoc_rejects_exact_request_with_smaller_minimum() {
        assert_eq!(
            Ext4AllocationRequest::new(
                LogicalBlock::new(13),
                None,
                BlockCount::new(4),
                BlockCount::new(2),
                Ext4AllocationFlags::EXACT,
                BlockGroupNumber::new(0),
            ),
            Err(Ext4Error::OutOfBounds)
        );
    }

    #[test]
    fn block_allocator_releases_allocated_block_through_same_metadata_path() {
        let (mut filesystem, device) = allocator_test_filesystem(25, 0b0111_1111);
        let journal = JournalTransactions::new(TransactionId::new(201));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();

        let released = filesystem
            .release_allocated_block(PhysicalBlock::new(6), &mut handle)
            .unwrap();

        assert_eq!(released.block(), PhysicalBlock::new(6));
        assert_eq!(released.bitmap_bit(), 6);
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 26);
        assert_eq!(filesystem.superblock().free_blocks_count(), 26);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0);
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 26);
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 26);
    }

    #[test]
    fn block_allocator_releases_metadata_block_with_revoke_record() {
        let (mut filesystem, device) = allocator_test_filesystem(25, 0b0111_1111);
        let journal = JournalTransactions::new(TransactionId::new(202));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();

        let released = filesystem
            .release_allocated_metadata_block(PhysicalBlock::new(6), &mut handle)
            .unwrap();

        assert_eq!(released.block(), PhysicalBlock::new(6));
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 26);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 4);
        assert_eq!(
            commit.metadata_blocks().unwrap().as_ref(),
            &[
                FilesystemBlock::new(0),
                FilesystemBlock::new(1),
                FilesystemBlock::new(2),
            ]
        );
        assert_eq!(
            commit.revoked_blocks().unwrap().as_ref(),
            &[FilesystemBlock::new(6)]
        );

        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0);
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 26);
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 26);
    }

    #[test]
    fn expected_error_before_metadata_access_does_not_poison_mount_journal() {
        let (mut filesystem, _device) = journal_allocator_test_filesystem(0, 0xff);
        install_test_internal_journal(&mut filesystem, 250);
        let journal = filesystem.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();

        assert_eq!(
            filesystem.allocate_block(None, &mut handle),
            Err(Ext4Error::NoSpace)
        );
        assert!(!handle.has_updates());
        assert_eq!(
            filesystem.complete_metadata_mutation(handle, Err::<(), _>(Ext4Error::NoSpace)),
            Err(Ext4Error::NoSpace)
        );
        assert_eq!(filesystem.groups()[0].free_blocks_count(), 0);
        assert!(!journal.is_aborted());
        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));

        let retry = journal.begin(JournalCredits::new(4)).unwrap();
        assert_eq!(retry.id(), transaction);
        drop(retry);
    }

    #[test]
    fn expected_error_on_empty_handle_does_not_abort_other_active_handle() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 251);
        let journal = filesystem.metadata_journal().unwrap();

        let mut first = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = first.id();
        let allocation = filesystem.allocate_block(None, &mut first).unwrap();
        let second = journal.begin(JournalCredits::new(1)).unwrap();
        assert_eq!(
            filesystem.complete_metadata_mutation(second, Err::<(), _>(Ext4Error::NoSpace)),
            Err(Ext4Error::NoSpace)
        );
        assert!(!journal.is_aborted());
        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));
        assert_eq!(
            filesystem.groups()[0].free_blocks_count(),
            TEST_FREE_BLOCKS - 1
        );
        assert_eq!(allocation.block(), PhysicalBlock::new(6));

        filesystem
            .complete_metadata_mutation(first, Ok(()))
            .expect("finish surviving metadata mutation");
        assert!(!journal.is_aborted());
    }

    #[test]
    fn successful_handle_can_finish_while_another_handle_remains_active() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 252);
        let journal = filesystem.metadata_journal().unwrap();

        let mut first = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = first.id();
        let second = journal.begin(JournalCredits::new(1)).unwrap();
        filesystem.allocate_block(None, &mut first).unwrap();

        filesystem
            .complete_metadata_mutation(first, Ok(()))
            .expect("finish first handle while second remains active");
        assert!(!journal.is_aborted());
        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));

        filesystem
            .complete_metadata_mutation(second, Ok(()))
            .expect("finish last handle below the transaction limit");
        assert!(!journal.is_aborted());
        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));
    }

    #[test]
    fn expected_error_after_metadata_access_aborts_without_rewinding_state() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 253);
        let journal = filesystem.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        let allocation = filesystem.allocate_block(None, &mut handle).unwrap();
        assert_eq!(allocation.block(), PhysicalBlock::new(6));
        assert_eq!(
            filesystem.groups()[0].free_blocks_count(),
            TEST_FREE_BLOCKS - 1
        );

        assert_eq!(
            filesystem.complete_metadata_mutation(handle, Err::<(), _>(Ext4Error::NoSpace)),
            Err(Ext4Error::InvalidJournalTransaction)
        );
        assert!(journal.is_aborted());
        assert_eq!(
            filesystem.groups()[0].free_blocks_count(),
            TEST_FREE_BLOCKS - 1
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            u64::from(TEST_FREE_BLOCKS - 1)
        );
        assert!(matches!(
            journal.begin(JournalCredits::new(1)),
            Err(Ext4Error::JournalAborted)
        ));
    }

    #[test]
    fn commit_failure_after_metadata_publication_aborts_mount_journal() {
        let (mut filesystem, device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 254);
        let journal = filesystem.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        filesystem.allocate_block(None, &mut handle).unwrap();
        device.fail_flush_at(device.flush_count() + 1);

        assert_eq!(
            filesystem.complete_metadata_mutation_with_policy(
                handle,
                Ok(()),
                crate::journal::RecoveryFlagPolicy::PreserveDuringRecovery,
            ),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert!(journal.is_aborted());
        assert!(matches!(
            journal.begin(JournalCredits::new(1)),
            Err(Ext4Error::JournalAborted)
        ));
    }

    #[test]
    fn explicit_sync_commit_failure_aborts_mount_journal() {
        let (mut filesystem, device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 255);
        let journal = filesystem.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .complete_metadata_mutation(handle, Ok(()))
            .expect("leave successful metadata in the running transaction");
        device.fail_flush_at(device.flush_count() + 1);

        assert_eq!(
            filesystem.sync_filesystem(),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert!(journal.is_aborted());
        assert_eq!(filesystem.sync_filesystem(), Err(Ext4Error::JournalAborted));
        assert!(matches!(
            journal.begin(JournalCredits::new(1)),
            Err(Ext4Error::JournalAborted)
        ));
    }

    #[test]
    fn failed_handle_reusing_shared_metadata_aborts_mount_journal() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 256);
        let journal = filesystem.metadata_journal().unwrap();

        let mut first = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = first.id();
        filesystem.allocate_block(None, &mut first).unwrap();
        filesystem
            .complete_metadata_mutation(first, Ok(()))
            .expect("retain the first allocation in the running transaction");

        let mut second = journal.begin(JournalCredits::new(4)).unwrap();
        assert_eq!(second.id(), transaction);
        filesystem.allocate_block(None, &mut second).unwrap();
        assert!(second.has_updates());
        assert_eq!(
            filesystem.complete_metadata_mutation(second, Err::<(), _>(Ext4Error::NoSpace)),
            Err(Ext4Error::InvalidJournalTransaction)
        );
        assert!(journal.is_aborted());
    }

    #[test]
    fn block_allocator_rejects_releasing_system_zone_without_consuming_credits() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(301));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.release_allocated_block(PhysicalBlock::new(2), &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
    }

    #[test]
    fn block_allocator_rejects_insufficient_credits_before_metadata_access() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(311));
        let mut handle = journal.begin(JournalCredits::new(2)).unwrap();

        assert_eq!(
            filesystem.allocate_block(None, &mut handle),
            Err(Ext4Error::InsufficientJournalCredits)
        );
        assert_eq!(handle.remaining_credits(), 2);
        assert_eq!(filesystem.groups()[0].free_blocks_count(), TEST_FREE_BLOCKS);
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            u64::from(TEST_FREE_BLOCKS)
        );
    }

    #[test]
    fn inode_allocator_rejects_insufficient_credits_before_metadata_access() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(312));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle
            ),
            Err(Ext4Error::InsufficientJournalCredits)
        );
        assert_eq!(handle.remaining_credits(), 3);
        assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
        assert_eq!(
            filesystem.superblock().free_inodes_count(),
            TEST_FREE_INODES
        );
    }

    #[test]
    fn ext4_bitmap_tail_bits_are_marked_used() {
        let mut bitmap = [0; 2];

        ext4_mark_bitmap_end(10, 16, &mut bitmap).unwrap();

        assert_eq!(bitmap, [0, 0b1111_1100]);
    }

    #[test]
    fn bitmap_checksum_compare_uses_descriptor_width() {
        assert!(ext4_bitmap_checksum_matches(
            0xaaaa_1234,
            0xbbbb_1234,
            false
        ));
        assert!(!ext4_bitmap_checksum_matches(
            0xaaaa_1234,
            0xbbbb_5678,
            false
        ));
        assert!(!ext4_bitmap_checksum_matches(
            0xaaaa_1234,
            0xbbbb_1234,
            true
        ));
        assert!(ext4_bitmap_checksum_matches(0xaaaa_1234, 0xaaaa_1234, true));
    }

    #[test]
    fn block_allocator_rejects_uninit_group_zero() {
        let (mut filesystem, _device) = allocator_test_filesystem_with_flags(
            TEST_FREE_BLOCKS,
            0,
            TEST_FREE_INODES,
            0x03,
            0,
            TEST_EXT4_BG_BLOCK_UNINIT,
        );
        let journal = JournalTransactions::new(TransactionId::new(321));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
    }

    #[test]
    fn block_allocator_lazy_initializes_nonzero_group_bitmap() {
        let (mut filesystem, device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: 0,
                free_inodes: 0,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0xff; 4],
                inode_bitmap: [0xff; 4],
            },
            AllocatorGroupSpec {
                free_blocks: 32,
                free_inodes: 0,
                used_directories: 0,
                flags: TEST_EXT4_BG_BLOCK_UNINIT,
                block_bitmap: [0; 4],
                inode_bitmap: [0xff; 4],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(324));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();

        let allocation = filesystem
            .allocate_block(Some(FilesystemBlock::new(38)), &mut handle)
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.block(), PhysicalBlock::new(38));
        assert_eq!(filesystem.groups()[1].free_blocks_count(), 25);
        assert_eq!(filesystem.groups()[1].flags(), 0);
        assert_eq!(filesystem.superblock().free_blocks_count(), 25);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        let group1_descriptor_offset = TEST_BLOCK_SIZE + 64;
        let group1_block_bitmap_offset = (TEST_BLOCK_COUNT + 2) * TEST_BLOCK_SIZE;
        assert_eq!(
            le_u16(&bytes, group1_descriptor_offset + 18) & TEST_EXT4_BG_BLOCK_UNINIT,
            0
        );
        assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 25);
        assert_eq!(bytes[group1_block_bitmap_offset], 0b0111_1111);
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 25);
    }

    #[test]
    fn mballoc_run_allocation_lazy_initializes_nonzero_group_bitmap() {
        let (mut filesystem, device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: 0,
                free_inodes: 0,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0xff; 4],
                inode_bitmap: [0xff; 4],
            },
            AllocatorGroupSpec {
                free_blocks: 32,
                free_inodes: 0,
                used_directories: 0,
                flags: TEST_EXT4_BG_BLOCK_UNINIT,
                block_bitmap: [0; 4],
                inode_bitmap: [0xff; 4],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(334));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();
        let request = Ext4AllocationRequest::new(
            LogicalBlock::new(20),
            Some(FilesystemBlock::new(38)),
            BlockCount::new(4),
            BlockCount::new(4),
            Ext4AllocationFlags::EXACT,
            BlockGroupNumber::new(1),
        )
        .unwrap();

        let allocation = filesystem
            .allocate_blocks_for_write(request, &mut handle)
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.physical_start(), PhysicalBlock::new(38));
        assert_eq!(allocation.block_count(), BlockCount::new(4));
        assert_eq!(filesystem.groups()[1].free_blocks_count(), 22);
        assert_eq!(filesystem.groups()[1].flags(), 0);
        assert_eq!(filesystem.superblock().free_blocks_count(), 22);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        let group1_descriptor_offset = TEST_BLOCK_SIZE + 64;
        let group1_block_bitmap_offset = (TEST_BLOCK_COUNT + 2) * TEST_BLOCK_SIZE;
        assert_eq!(
            le_u16(&bytes, group1_descriptor_offset + 18) & TEST_EXT4_BG_BLOCK_UNINIT,
            0
        );
        assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 22);
        assert_eq!(bytes[group1_block_bitmap_offset], 0xff);
        assert_eq!(
            bytes[group1_block_bitmap_offset + 1] & 0b0000_0011,
            0b0000_0011
        );
        assert_eq!(le_u32(&bytes, 1024 + 0x0c), 22);
    }

    #[test]
    fn block_allocator_rejects_initialized_bitmap_with_cleared_tail_bit() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        device.bytes.lock().unwrap()[2 * TEST_BLOCK_SIZE + 4] = 0;
        let journal = JournalTransactions::new(TransactionId::new(325));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(
            filesystem.allocate_block(None, &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
        assert_eq!(device.bytes.lock().unwrap()[2 * TEST_BLOCK_SIZE + 4], 0);
    }

    #[test]
    fn block_allocator_rejects_initialized_bitmap_with_cleared_system_zone_bit() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1011);
        let journal = JournalTransactions::new(TransactionId::new(326));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
        assert_eq!(
            device.bytes.lock().unwrap()[2 * TEST_BLOCK_SIZE],
            0b0011_1011
        );
    }

    #[test]
    fn block_allocator_rejects_initialized_bitmap_checksum_mismatch() {
        let (mut filesystem, device) = allocator_test_filesystem_with_options(
            TEST_FREE_BLOCKS,
            0b0011_1111,
            TEST_FREE_INODES,
            0x03,
            0,
            0,
            true,
            true,
            false,
        );
        let journal = JournalTransactions::new(TransactionId::new(327));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
        assert_eq!(
            device.bytes.lock().unwrap()[2 * TEST_BLOCK_SIZE],
            0b0011_1111
        );
    }

    #[test]
    fn block_allocator_release_rejects_initialized_bitmap_checksum_mismatch() {
        let (mut filesystem, device) = allocator_test_filesystem_with_options(
            25,
            0b0111_1111,
            TEST_FREE_INODES,
            0x03,
            0,
            0,
            true,
            true,
            false,
        );
        let journal = JournalTransactions::new(TransactionId::new(328));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        assert_eq!(
            filesystem.release_allocated_block(PhysicalBlock::new(6), &mut handle),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
        );
        assert_eq!(handle.remaining_credits(), 3);
        assert_eq!(
            device.bytes.lock().unwrap()[2 * TEST_BLOCK_SIZE],
            0b0111_1111
        );
    }

    #[test]
    fn block_allocator_skips_bitmap_checksum_without_metadata_csum() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(329));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

        let allocation = filesystem
            .allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle)
            .unwrap();

        assert_eq!(allocation.block(), PhysicalBlock::new(6));
    }

    #[test]
    fn inode_allocator_rejects_uninit_group_zero() {
        let (mut filesystem, _device) = allocator_test_filesystem_with_flags(
            TEST_FREE_BLOCKS,
            0b0011_1111,
            TEST_FREE_INODES,
            0,
            0,
            TEST_EXT4_BG_INODE_UNINIT,
        );
        let journal = JournalTransactions::new(TransactionId::new(322));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        assert_eq!(
            filesystem.allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(handle.remaining_credits(), 4);
    }

    #[test]
    fn inode_allocator_rejects_initialized_bitmap_checksum_mismatch() {
        let (mut filesystem, device) = allocator_test_filesystem_with_options(
            TEST_FREE_BLOCKS,
            0b0011_1111,
            TEST_FREE_INODES,
            0x03,
            0,
            0,
            true,
            false,
            true,
        );
        let journal = JournalTransactions::new(TransactionId::new(330));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        assert_eq!(
            filesystem.allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(handle.remaining_credits(), 4);
        assert_eq!(device.bytes.lock().unwrap()[3 * TEST_BLOCK_SIZE + 1], 0x03);
    }

    #[test]
    fn inode_allocator_release_rejects_initialized_bitmap_checksum_mismatch() {
        let (mut filesystem, device) = allocator_test_filesystem_with_options(
            TEST_FREE_BLOCKS,
            0b0011_1111,
            TEST_FREE_INODES - 1,
            0x07,
            0,
            0,
            true,
            false,
            true,
        );
        let journal = JournalTransactions::new(TransactionId::new(331));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        assert_eq!(
            filesystem.release_allocated_inode(
                InodeNumber::new(11),
                InodeKind::RegularFile,
                &mut handle
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(handle.remaining_credits(), 4);
        assert_eq!(device.bytes.lock().unwrap()[3 * TEST_BLOCK_SIZE + 1], 0x07);
    }

    #[test]
    fn inode_allocator_skips_bitmap_checksum_without_metadata_csum() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(332));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        let allocation = filesystem
            .allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.inode(), InodeNumber::new(11));
    }

    #[test]
    fn inode_allocator_lazy_initializes_same_group_block_bitmap() {
        let (mut filesystem, device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: 0,
                free_inodes: 0,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0xff; 4],
                inode_bitmap: [0xff; 4],
            },
            AllocatorGroupSpec {
                free_blocks: 32,
                free_inodes: 32,
                used_directories: 0,
                flags: TEST_EXT4_BG_INODE_UNINIT | TEST_EXT4_BG_BLOCK_UNINIT,
                block_bitmap: [0; 4],
                inode_bitmap: [0; 4],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(323));
        let mut handle = journal.begin(JournalCredits::new(5)).unwrap();
        let transaction = handle.id();

        let allocation = filesystem
            .allocate_inode(
                Some(InodeNumber::new(33)),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.inode(), InodeNumber::new(33));
        assert_eq!(filesystem.groups()[1].free_blocks_count(), TEST_FREE_BLOCKS);
        assert_eq!(filesystem.groups()[1].free_inodes_count(), 31);
        assert_eq!(filesystem.groups()[1].flags(), 0);
        assert_eq!(filesystem.superblock().free_blocks_count(), 26);
        assert_eq!(filesystem.superblock().free_inodes_count(), 31);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        let group1_descriptor_offset = TEST_BLOCK_SIZE + 64;
        let group1_block_bitmap_offset = (TEST_BLOCK_COUNT + 2) * TEST_BLOCK_SIZE;
        let group1_inode_bitmap_offset = (TEST_BLOCK_COUNT + 3) * TEST_BLOCK_SIZE;
        assert_eq!(
            le_u16(&bytes, group1_descriptor_offset + 18)
                & (TEST_EXT4_BG_INODE_UNINIT | TEST_EXT4_BG_BLOCK_UNINIT),
            0
        );
        assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 26);
        assert_eq!(bytes[group1_block_bitmap_offset], 0b0011_1111);
        assert_eq!(bytes[group1_inode_bitmap_offset], 0b0000_0001);
    }

    #[test]
    fn inode_allocator_rejects_initialized_bitmap_with_cleared_tail_bit() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        device.bytes.lock().unwrap()[3 * TEST_BLOCK_SIZE + 4] = 0;
        let journal = JournalTransactions::new(TransactionId::new(327));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        assert_eq!(
            filesystem.allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(
            filesystem.allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(handle.remaining_credits(), 4);
        assert_eq!(device.bytes.lock().unwrap()[3 * TEST_BLOCK_SIZE + 4], 0);
    }

    #[test]
    fn inode_allocator_skips_group_with_cleared_reserved_inode_bit() {
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0xfe, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: 32,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0, 0, 0, 0],
            },
        ]);
        let journal = JournalTransactions::new(TransactionId::new(328));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        let allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(1));
        assert_eq!(allocation.inode(), InodeNumber::new(33));
        assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
        assert_eq!(filesystem.groups()[1].free_inodes_count(), 31);
    }

    #[test]
    fn inode_allocator_journals_bitmap_group_and_superblock_updates() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(401));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();

        let allocation = filesystem
            .allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::regular_file(0o644, 0, 0)
                    .with_owner(1000, 1001)
                    .with_timestamp_seconds(123)
                    .with_generation(77),
                &mut handle,
            )
            .unwrap();

        assert_eq!(allocation.group(), BlockGroupNumber::new(0));
        assert_eq!(allocation.inode(), InodeNumber::new(11));
        assert_eq!(allocation.bitmap_bit(), 10);
        assert_eq!(filesystem.groups()[0].free_inodes_count(), 21);
        assert_eq!(filesystem.superblock().free_inodes_count(), 21);
        let inode = filesystem.internal_inode(allocation.inode()).unwrap();
        assert_eq!(inode.kind(), InodeKind::RegularFile);
        assert_eq!(inode.mode(), 0o100644);
        assert_eq!(inode.uid(), 1000);
        assert_eq!(inode.gid(), 1001);
        assert_eq!(inode.links_count(), 1);
        assert_eq!(inode.generation(), 77);
        assert_eq!(inode.mtime(), crate::Ext4Timestamp::new(123, 0));
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 4);
        assert_eq!(
            commit.metadata_blocks().unwrap().as_ref(),
            &[
                FilesystemBlock::new(0),
                FilesystemBlock::new(1),
                FilesystemBlock::new(3),
                FilesystemBlock::new(4),
            ]
        );
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[3 * TEST_BLOCK_SIZE + 1], 0x07);
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 14), 21);
        assert_eq!(le_u32(&bytes, 1024 + 0x10), 21);
        let inode_offset = 4 * TEST_BLOCK_SIZE + 10 * 256;
        assert_eq!(le_u16(&bytes, inode_offset), 0o100644);
        assert_eq!(
            le_u32(&bytes, inode_offset + 0x20),
            crate::disk::inode::EXT4_EXTENTS_FL
        );
        assert_eq!(
            le_u16(&bytes, inode_offset + 0x28),
            crate::disk::extent::EXTENT_MAGIC
        );
    }

    #[test]
    fn inline_extent_mutation_journals_inode_table_update() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(451));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let inode = filesystem
            .internal_inode(inode_allocation.inode())
            .expect("read initialized inode");

        let updated_inode = filesystem
            .insert_inline_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .expect("insert inline extent mapping");

        assert_eq!(
            filesystem.map_blocks(&updated_inode, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical: block.block(),
                len: BlockCount::new(1),
            })
        );
        assert_eq!(handle.remaining_credits(), 3);
    }

    #[test]
    fn extent_root_grow_without_revoke_journals_only_live_leaf_block() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(461));
        let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
        let transaction = handle.id();
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem
            .internal_inode(inode_allocation.inode())
            .expect("read initialized inode");
        let mut extents = Vec::new();

        for logical in [0, 2, 4, 6, 8, 10] {
            let block = filesystem.allocate_block(None, &mut handle).unwrap();
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(logical),
                    block.block(),
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .expect("insert extent mapping");
            extents.push((logical, block.block()));
        }

        assert_eq!(le_u16(inode.extent_bytes(), 0x02), 1);
        assert_eq!(le_u16(inode.extent_bytes(), 0x06), 1);
        let index_offset = crate::disk::extent::EXTENT_HEADER_SIZE;
        assert_eq!(le_u32(inode.extent_bytes(), index_offset), 0);
        let extent_block = u64::from(le_u32(inode.extent_bytes(), index_offset + 0x04))
            | (u64::from(le_u16(inode.extent_bytes(), index_offset + 0x08)) << 32);
        assert!(filesystem.is_system_zone_block(FilesystemBlock::new(extent_block)));

        for (logical, physical) in extents {
            assert_eq!(
                filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
                Ok(crate::BlockMapping::Mapped {
                    physical,
                    len: BlockCount::new(1),
                })
            );
        }
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert!(commit.revoked_blocks().unwrap().as_ref().is_empty());
        assert!(
            commit
                .metadata_blocks()
                .unwrap()
                .as_ref()
                .contains(&FilesystemBlock::new(extent_block))
        );
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        let extent_block_offset = usize::try_from(extent_block).unwrap() * TEST_BLOCK_SIZE;
        assert_eq!(
            le_u16(&bytes, extent_block_offset),
            crate::disk::extent::EXTENT_MAGIC
        );
        assert_eq!(le_u16(&bytes, extent_block_offset + 0x02), 6);
        assert_eq!(le_u16(&bytes, extent_block_offset + 0x06), 0);
    }

    #[test]
    fn extent_point_mutations_keep_existing_leaf_block() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(4611));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem
            .internal_inode(inode_allocation.inode())
            .expect("read initialized inode");

        for logical in [0, 2, 4, 6, 8] {
            let block = filesystem.allocate_block(None, &mut handle).unwrap();
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(logical),
                    block.block(),
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
        }
        let index_offset = crate::disk::extent::EXTENT_HEADER_SIZE;
        let extent_block = u64::from(le_u32(inode.extent_bytes(), index_offset + 0x04))
            | (u64::from(le_u16(inode.extent_bytes(), index_offset + 0x08)) << 32);

        let data = filesystem.allocate_block(None, &mut handle).unwrap();
        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(10),
                data.block(),
                BlockCount::new(1),
                ExtentMappingState::Unwritten,
                &mut handle,
            )
            .unwrap();
        inode = filesystem
            .convert_unwritten_extent_range(
                &inode,
                LogicalBlock::new(10),
                BlockCount::new(1),
                &mut handle,
            )
            .unwrap();
        inode = filesystem
            .remove_extent_range(
                &inode,
                LogicalBlock::new(10),
                BlockCount::new(1),
                &mut handle,
            )
            .unwrap();

        let current_extent_block = u64::from(le_u32(inode.extent_bytes(), index_offset + 0x04))
            | (u64::from(le_u16(inode.extent_bytes(), index_offset + 0x08)) << 32);
        assert_eq!(current_extent_block, extent_block);
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(10)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
    }

    #[test]
    fn extent_leaf_split_leaves_space_in_reused_leaf() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        }; 16];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        let journal = JournalTransactions::new(TransactionId::new(4612));
        let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem
            .internal_inode(inode_allocation.inode())
            .expect("read initialized inode");
        let leaf_capacity =
            u64::from(crate::disk::extent::extent_block_capacity(TEST_BLOCK_SIZE).unwrap());

        for logical in 0..=leaf_capacity {
            let block = filesystem.allocate_block(None, &mut handle).unwrap();
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(logical * 2),
                    block.block(),
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
        }
        assert_eq!(le_u16(inode.extent_bytes(), 0x02), 2);

        let inserted = filesystem.allocate_block(None, &mut handle).unwrap();
        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(1),
                inserted.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();

        assert_eq!(le_u16(inode.extent_bytes(), 0x02), 2);
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(1)),
            Ok(crate::BlockMapping::Mapped {
                physical: inserted.block(),
                len: BlockCount::new(1),
            })
        );
    }

    #[test]
    fn extent_unwritten_conversion_splits_only_requested_range() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(462));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();

        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                physical,
                BlockCount::new(6),
                ExtentMappingState::Unwritten,
                &mut handle,
            )
            .unwrap();
        inode = filesystem
            .convert_unwritten_extent_range(
                &inode,
                LogicalBlock::new(2),
                BlockCount::new(2),
                &mut handle,
            )
            .unwrap();

        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Unwritten {
                physical,
                len: BlockCount::new(2),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(2)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(physical.get() + 2),
                len: BlockCount::new(2),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(4)),
            Ok(crate::BlockMapping::Unwritten {
                physical: PhysicalBlock::new(physical.get() + 4),
                len: BlockCount::new(2),
            })
        );
    }

    #[test]
    fn allocation_goal_after_previous_extent_uses_requested_logical_block() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(463));
        let mut handle = journal.begin(JournalCredits::new(128)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 10, &mut handle);
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();
        let inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(10),
                physical,
                BlockCount::new(10),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();

        assert_eq!(
            filesystem
                .allocation_goal_after_previous_extent(&inode, LogicalBlock::new(15))
                .unwrap(),
            Some(FilesystemBlock::new(physical.get() + 5))
        );
    }

    #[test]
    fn extent_remove_range_splits_mapping_and_releases_data_blocks() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(463));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();
        let free_after_allocation = filesystem.superblock().free_blocks_count();

        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                physical,
                BlockCount::new(6),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
        inode = filesystem
            .remove_extent_range(
                &inode,
                LogicalBlock::new(2),
                BlockCount::new(2),
                &mut handle,
            )
            .unwrap();

        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical,
                len: BlockCount::new(2),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(2)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(2),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(4)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(physical.get() + 4),
                len: BlockCount::new(2),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_after_allocation + 2
        );
    }

    #[test]
    fn extent_remove_range_splits_full_inline_leaf() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(4631));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 3, &mut handle);
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();
        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                physical,
                BlockCount::new(3),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
        for logical in [4, 6, 8] {
            let block = filesystem.allocate_block(None, &mut handle).unwrap();
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(logical),
                    block.block(),
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
        }

        inode = filesystem
            .remove_extent_range(
                &inode,
                LogicalBlock::new(1),
                BlockCount::new(1),
                &mut handle,
            )
            .unwrap();

        assert_eq!(le_u16(inode.extent_bytes(), 0x06), 1);
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(1)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(1),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(2)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(physical.get() + 2),
                len: BlockCount::new(1),
            })
        );
    }

    #[test]
    fn extent_truncate_releases_tail_range() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(464));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();
        let free_after_allocation = filesystem.superblock().free_blocks_count();

        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                physical,
                BlockCount::new(6),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
        inode = filesystem
            .truncate_extent_mappings(&inode, LogicalBlock::new(3), &mut handle)
            .unwrap();

        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical,
                len: BlockCount::new(3),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(3)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_after_allocation + 3
        );
    }

    #[test]
    fn extent_mutation_splits_multiple_leaf_blocks() {
        let mut groups = [AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        }; 16];
        groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
        let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
        let journal = JournalTransactions::new(TransactionId::new(465));
        let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();
        let mut mappings = Vec::new();

        for logical in 0..350u64 {
            let block = filesystem.allocate_block(None, &mut handle).unwrap();
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(logical * 2),
                    block.block(),
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
            mappings.push((logical * 2, block.block()));
        }

        assert_eq!(le_u16(inode.extent_bytes(), 0x02), 2);
        assert_eq!(le_u16(inode.extent_bytes(), 0x06), 1);
        for (logical, physical) in mappings {
            assert_eq!(
                filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
                Ok(crate::BlockMapping::Mapped {
                    physical,
                    len: BlockCount::new(1),
                })
            );
        }
    }

    #[test]
    fn ordered_writeback_allocates_hole_and_commits_written_size_before_final_size() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 701);
        let input = vec![0x5a; TEST_BLOCK_SIZE];
        let visible_size = u64::try_from(TEST_BLOCK_SIZE * 2).unwrap();
        let timestamp = crate::Ext4Timestamp::new(1234, 0);

        let updated = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                visible_size,
                timestamp,
                Ext4SyncIntent::DataOnly,
            )
            .unwrap();

        assert_eq!(updated.size(), TEST_BLOCK_SIZE as u64);
        assert_eq!(updated.blocks(), (TEST_BLOCK_SIZE / 512) as u64);
        assert_ne!(updated.ctime(), timestamp);
        assert_ne!(updated.mtime(), timestamp);
        assert_eq!(
            filesystem.map_blocks(&updated, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(6),
                len: BlockCount::new(1),
            })
        );
        let mut output = vec![0; TEST_BLOCK_SIZE];
        assert_eq!(
            filesystem.read_at(&updated, 0, &mut output).unwrap(),
            TEST_BLOCK_SIZE
        );
        assert_eq!(output, input);

        let final_inode = filesystem
            .commit_regular_inode_write_metadata(
                &updated,
                visible_size,
                RegularWriteMetadata::Full { timestamp },
            )
            .unwrap();
        assert_eq!(final_inode.size(), visible_size);
        assert_eq!(final_inode.blocks(), (TEST_BLOCK_SIZE / 512) as u64);
        assert_eq!(final_inode.ctime(), timestamp);
        assert_eq!(final_inode.mtime(), timestamp);
        assert_eq!(
            filesystem.commit_regular_inode_write_metadata(
                &final_inode,
                visible_size - 1,
                RegularWriteMetadata::SizeOnly,
            ),
            Err(Ext4Error::Unsupported(UnsupportedKind::FileSizeShrink))
        );
        let mut sparse_tail = [0xff];
        assert_eq!(
            filesystem
                .read_at(&final_inode, visible_size - 1, &mut sparse_tail)
                .unwrap(),
            1
        );
        assert_eq!(sparse_tail, [0]);
    }

    #[test]
    fn ordered_writeback_append_preserves_existing_partial_block_data() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 711);
        let timestamp = crate::Ext4Timestamp::new(5678, 0);

        let inode = filesystem
            .writeback_ordered_at(&inode, 0, b"abc", 3, timestamp, Ext4SyncIntent::DataOnly)
            .unwrap();
        let inode = filesystem
            .writeback_ordered_at(&inode, 3, b"def", 6, timestamp, Ext4SyncIntent::DataOnly)
            .unwrap();

        let mut output = vec![0; 6];
        assert_eq!(filesystem.read_at(&inode, 0, &mut output).unwrap(), 6);
        assert_eq!(&output, b"abcdef");
        assert_eq!(inode.size(), 6);
    }

    #[test]
    fn ordered_writeback_preallocates_and_discards_unwritten_tail() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 712);
        let input = vec![0x42; TEST_BLOCK_SIZE * 4];
        let free_before_write = filesystem.superblock().free_blocks_count();

        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(5680, 0),
                Ext4SyncIntent::DataOnly,
            )
            .unwrap();

        assert_eq!(written.size(), input.len() as u64);
        assert_eq!(written.blocks(), (TEST_BLOCK_SIZE / 512 * 8) as u64);
        assert_eq!(
            filesystem.map_blocks(&written, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(6),
                len: BlockCount::new(4),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&written, LogicalBlock::new(4)),
            Ok(crate::BlockMapping::Unwritten {
                physical: PhysicalBlock::new(10),
                len: BlockCount::new(4),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_write - 8
        );
        assert!(
            filesystem
                .extent_truncate_metadata_credits(&written, LogicalBlock::new(4))
                .unwrap()
                <= 16,
            "discard credits must follow the mapped tail structure, not total inode blocks"
        );

        install_test_internal_journal(&mut filesystem, 713);
        let discarded = filesystem
            .discard_regular_inode_preallocations(&written)
            .unwrap();

        assert_eq!(discarded.blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
        assert_eq!(
            filesystem.map_blocks(&discarded, LogicalBlock::new(4)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_write - 4
        );
    }

    #[test]
    fn ordered_writeback_prealloc_budget_zero_allocates_only_dirty_blocks() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 713);
        let input = vec![0x34; TEST_BLOCK_SIZE * 4];
        let free_before_write = filesystem.superblock().free_blocks_count();

        let written = filesystem
            .writeback_ordered_at_with_prealloc_budget(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(5683, 0),
                Ext4SyncIntent::DataOnly,
                0,
            )
            .unwrap();

        assert_eq!(written.blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
        assert_eq!(
            filesystem.map_blocks(&written, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(6),
                len: BlockCount::new(4),
            })
        );
        assert_eq!(
            filesystem.map_blocks(&written, LogicalBlock::new(4)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_write - 4
        );
    }

    #[test]
    fn ordered_writeback_reuses_preallocated_unwritten_tail() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 714);
        let first = vec![0x41; TEST_BLOCK_SIZE * 4];
        let second = vec![0x62; TEST_BLOCK_SIZE * 4];
        let free_before_write = filesystem.superblock().free_blocks_count();

        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &first,
                first.len() as u64,
                crate::Ext4Timestamp::new(5681, 0),
                Ext4SyncIntent::DataOnly,
            )
            .unwrap();
        install_test_internal_journal(&mut filesystem, 715);
        let appended = filesystem
            .writeback_ordered_at(
                &written,
                first.len() as u64,
                &second,
                (first.len() + second.len()) as u64,
                crate::Ext4Timestamp::new(5682, 0),
                Ext4SyncIntent::DataOnly,
            )
            .unwrap();

        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_write - 8
        );
        assert_eq!(
            filesystem.map_blocks(&appended, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical: PhysicalBlock::new(6),
                len: BlockCount::new(8),
            })
        );
        let mut output = vec![0; TEST_BLOCK_SIZE * 8];
        assert_eq!(
            filesystem.read_at(&appended, 0, &mut output).unwrap(),
            output.len()
        );
        assert_eq!(&output[..first.len()], &first);
        assert_eq!(&output[first.len()..], &second);
    }

    #[test]
    fn ordered_writeback_converts_unwritten_extent_and_zero_fills_partial_block() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(721));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let transaction = handle.id();
        let physical = allocate_contiguous_blocks(&mut filesystem, 1, &mut handle);
        let allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(allocation.inode()).unwrap();
        inode = filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(0),
                physical,
                BlockCount::new(1),
                ExtentMappingState::Unwritten,
                &mut handle,
            )
            .unwrap();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();
        install_test_internal_journal(&mut filesystem, 722);

        let updated = filesystem
            .writeback_ordered_at(
                &inode,
                2,
                b"xy",
                4,
                crate::Ext4Timestamp::new(9, 0),
                Ext4SyncIntent::DataOnly,
            )
            .unwrap();

        assert_eq!(
            filesystem.map_blocks(&updated, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Mapped {
                physical,
                len: BlockCount::new(1),
            })
        );
        let mut output = vec![0xff; 4];
        assert_eq!(filesystem.read_at(&updated, 0, &mut output).unwrap(), 4);
        assert_eq!(&output, &[0, 0, b'x', b'y']);
    }

    #[test]
    fn truncate_grow_keeps_new_range_sparse_and_zero_reading() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 731);
        let timestamp = crate::Ext4Timestamp::new(17, 0);
        let new_size = u64::try_from(TEST_BLOCK_SIZE * 2 + 7).unwrap();

        let updated = filesystem
            .truncate_regular_inode(&inode, new_size, timestamp)
            .unwrap();

        assert_eq!(updated.size(), new_size);
        assert_eq!(updated.ctime(), timestamp);
        assert_eq!(updated.mtime(), timestamp);
        assert_eq!(
            filesystem.map_blocks(&updated, LogicalBlock::new(0)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        let mut output = [0xff; 4];
        assert_eq!(
            filesystem
                .read_at(&updated, new_size - output.len() as u64, &mut output)
                .unwrap(),
            output.len()
        );
        assert_eq!(output, [0; 4]);
    }

    #[test]
    fn truncate_grow_zeroes_mapped_old_eof_tail() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 732);
        let input = vec![0x6d; TEST_BLOCK_SIZE];
        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(18, 0),
                Ext4SyncIntent::FullMetadata,
            )
            .unwrap();
        install_test_internal_journal(&mut filesystem, 733);
        let journal = JournalTransactions::new(TransactionId::new(734));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
        let transaction = handle.id();
        let stale_tail_inode = filesystem
            .update_regular_inode_size_metadata(
                &written,
                23,
                RegularWriteMetadata::SizeOnly,
                &mut handle,
            )
            .unwrap();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();
        install_test_internal_journal(&mut filesystem, 735);

        let grown = filesystem
            .truncate_regular_inode(
                &stale_tail_inode,
                TEST_BLOCK_SIZE as u64,
                crate::Ext4Timestamp::new(19, 0),
            )
            .unwrap();

        let mut output = vec![0xff; TEST_BLOCK_SIZE];
        assert_eq!(
            filesystem.read_at(&grown, 0, &mut output).unwrap(),
            TEST_BLOCK_SIZE
        );
        assert_eq!(&output[..23], &[0x6d; 23]);
        assert!(output[23..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn truncate_shrink_releases_tail_blocks_and_zeroes_partial_eof_block() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 741);
        let input = vec![0xa5; TEST_BLOCK_SIZE * 3];
        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(21, 0),
                Ext4SyncIntent::FullMetadata,
            )
            .unwrap();
        assert_eq!(written.blocks(), (TEST_BLOCK_SIZE / 512 * 3) as u64);
        install_test_internal_journal(&mut filesystem, 742);
        let free_before_truncate = filesystem.superblock().free_blocks_count();
        let new_size = u64::try_from(TEST_BLOCK_SIZE + 17).unwrap();

        let truncated = filesystem
            .truncate_regular_inode(&written, new_size, crate::Ext4Timestamp::new(22, 0))
            .unwrap();

        assert_eq!(truncated.size(), new_size);
        assert_eq!(truncated.blocks(), (TEST_BLOCK_SIZE / 512 * 2) as u64);
        assert_eq!(filesystem.orphan_head(), None);
        assert_eq!(
            filesystem.map_blocks(&truncated, LogicalBlock::new(2)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_truncate + 1
        );

        let crate::BlockMapping::Mapped { physical, .. } = filesystem
            .map_blocks(&truncated, LogicalBlock::new(1))
            .unwrap()
        else {
            panic!("partial EOF block should remain mapped");
        };
        let mut eof_block = vec![0; TEST_BLOCK_SIZE];
        filesystem
            .read_blocks(FilesystemBlock::new(physical.get()), 1, &mut eof_block)
            .unwrap();
        assert_eq!(&eof_block[..17], &[0xa5; 17]);
        assert!(eof_block[17..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn orphan_cleanup_finishes_committed_shrink_after_crash_point() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 751);
        let input = vec![0x5c; TEST_BLOCK_SIZE * 3];
        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(31, 0),
                Ext4SyncIntent::FullMetadata,
            )
            .unwrap();
        assert_eq!(written.blocks(), (TEST_BLOCK_SIZE / 512 * 3) as u64);
        install_test_internal_journal(&mut filesystem, 752);
        let free_before_orphan_cleanup = filesystem.superblock().free_blocks_count();
        let new_size = u64::try_from(TEST_BLOCK_SIZE + 9).unwrap();

        let journal = JournalTransactions::new(TransactionId::new(753));
        let mut handle = journal.begin(JournalCredits::new(16)).unwrap();
        let transaction = handle.id();
        let orphaned = filesystem.add_orphan(&written, &mut handle).unwrap();
        let orphaned = filesystem
            .update_regular_inode_size_metadata(
                &orphaned,
                new_size,
                RegularWriteMetadata::Full {
                    timestamp: crate::Ext4Timestamp::new(32, 0),
                },
                &mut handle,
            )
            .unwrap();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        assert_eq!(filesystem.orphan_head(), Some(written.number()));
        assert_eq!(orphaned.size(), new_size);

        assert_eq!(filesystem.cleanup_legacy_orphans().unwrap(), 1);

        let recovered = filesystem.internal_inode(written.number()).unwrap();
        assert_eq!(recovered.size(), new_size);
        assert_eq!(recovered.blocks(), (TEST_BLOCK_SIZE / 512 * 2) as u64);
        assert_eq!(filesystem.orphan_head(), None);
        assert_eq!(
            filesystem.map_blocks(&recovered, LogicalBlock::new(2)),
            Ok(crate::BlockMapping::Hole {
                len: BlockCount::new(u32::MAX),
            })
        );
        assert_eq!(
            filesystem.superblock().free_blocks_count(),
            free_before_orphan_cleanup + 1
        );
    }

    #[test]
    fn mount_rejects_clean_legacy_orphan_head_without_recovery() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        persist_test_orphan_head(&mut filesystem, Some(inode.number()), 764);
        drop(filesystem);

        let mount_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::mount(mount_device).map(|_| ()),
            Err(Ext4Error::NeedsRecovery)
        );
    }

    #[test]
    fn recover_rejects_clean_legacy_orphan_without_a_journal() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        persist_test_orphan_head(&mut filesystem, Some(inode.number()), 765);
        drop(filesystem);

        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::recover(recovery_device),
            Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))
        );
        let bytes = device.bytes();
        let superblock = Superblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
            .expect("decode persisted clean orphan head");
        assert_eq!(superblock.last_orphan(), inode.number().get());
    }

    #[test]
    fn truncate_rejects_orphan_file_feature() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 766);
        let input = vec![0x3f; TEST_BLOCK_SIZE];
        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                &input,
                input.len() as u64,
                crate::Ext4Timestamp::new(33, 0),
                Ext4SyncIntent::FullMetadata,
            )
            .unwrap();
        install_test_internal_journal(&mut filesystem, 767);
        set_allocator_feature_bits(
            &mut filesystem,
            features::CompatFeatures::ORPHAN_FILE,
            features::ReadOnlyCompatFeatures::empty(),
        );

        assert_eq!(
            filesystem.truncate_regular_inode(&written, 23, crate::Ext4Timestamp::new(34, 0)),
            Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile))
        );
    }

    #[test]
    fn cleanup_legacy_orphans_rejects_orphan_file_pending_feature() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        set_allocator_feature_bits(
            &mut filesystem,
            features::CompatFeatures::empty(),
            features::ReadOnlyCompatFeatures::ORPHAN_PRESENT,
        );

        assert_eq!(
            filesystem.cleanup_legacy_orphans(),
            Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile))
        );
    }

    #[test]
    fn regular_file_mutation_accepts_huge_file_feature_for_sector_accounted_inode() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 768);
        set_allocator_feature_bits(
            &mut filesystem,
            features::CompatFeatures::empty(),
            features::ReadOnlyCompatFeatures::HUGE_FILE,
        );

        let written = filesystem
            .writeback_ordered_at(
                &inode,
                0,
                b"x",
                1,
                crate::Ext4Timestamp::new(35, 0),
                Ext4SyncIntent::FullMetadata,
            )
            .expect("write ordinary inode on huge_file filesystem");
        filesystem
            .truncate_regular_inode(&written, 0, crate::Ext4Timestamp::new(36, 0))
            .expect("truncate ordinary inode on huge_file filesystem");
    }

    #[test]
    fn regular_file_mutation_rejects_inode_using_huge_file_accounting() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 769);
        set_allocator_feature_bits(
            &mut filesystem,
            features::CompatFeatures::empty(),
            features::ReadOnlyCompatFeatures::HUGE_FILE,
        );

        let journal = JournalTransactions::new(TransactionId::new(770));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let flagged = filesystem
            .update_inode_flags_timestamps_metadata(
                &inode,
                inode.flags() | crate::disk::inode::EXT4_HUGE_FILE_FL,
                crate::Ext4Timestamp::new(37, 0),
                &mut handle,
            )
            .expect("set huge-file inode flag");
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        assert_eq!(
            filesystem.writeback_ordered_at(
                &flagged,
                0,
                b"x",
                1,
                crate::Ext4Timestamp::new(38, 0),
                Ext4SyncIntent::FullMetadata,
            ),
            Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile))
        );
    }

    #[test]
    fn legacy_orphan_cleanup_preserving_recovery_evicts_zero_link_inode() {
        let (mut filesystem, _device) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_test_internal_journal(&mut filesystem, 769);
        let free_inodes_before_cleanup = filesystem.superblock().free_inodes_count();
        let journal = JournalTransactions::new(TransactionId::new(770));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
        let transaction = handle.id();
        let orphaned = filesystem.add_orphan(&inode, &mut handle).unwrap();
        let _unlinked = update_test_inode_links_count(&mut filesystem, &orphaned, 0, &mut handle);
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        assert_eq!(
            filesystem
                .cleanup_legacy_orphans_preserving_recovery()
                .unwrap(),
            1
        );
        assert_eq!(filesystem.orphan_head(), None);
        assert!(filesystem.superblock().features().needs_recovery());
        assert_eq!(filesystem.inode(inode.number()), Err(Ext4Error::NotFound));
        assert_eq!(
            filesystem.superblock().free_inodes_count(),
            free_inodes_before_cleanup + 1
        );
    }

    #[test]
    fn recovery_rebases_transaction_sequence_before_regular_orphan_cleanup() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let image = temporary_image_path("replay-then-orphan-cleanup");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated recovery image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated recovery image");
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        let journal = filesystem.metadata_journal().expect("open mounted journal");
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
        let transaction = handle.id();
        let orphaned = filesystem.add_orphan(&inode, &mut handle).unwrap();
        let _unlinked = update_test_inode_links_count(&mut filesystem, &orphaned, 0, &mut handle);
        drop(handle);
        let commit = journal.force_commit_for_test(transaction).unwrap();
        filesystem
            .persist_metadata_journal_commit(&commit)
            .expect("persist orphan transaction without checkpoint");
        drop(filesystem);

        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        let report = Ext4Filesystem::recover(recovery_device)
            .expect("replay and orphan cleanup")
            .expect("active journal report");
        assert!(report.update_count() > 0);

        let mount_device: Arc<dyn BlockDevice> = device.clone();
        let recovered = Ext4Filesystem::mount(mount_device).expect("mount recovered image");
        assert_eq!(recovered.orphan_head(), None);
        assert_eq!(recovered.inode(inode.number()), Err(Ext4Error::NotFound));
        assert!(!recovered.superblock().features().needs_recovery());
        fs::remove_file(image).expect("remove recovery image");
    }

    #[test]
    fn recover_keeps_recovery_feature_when_regular_orphan_cleanup_fails() {
        let mke2fs = require_e2fsprogs("mke2fs");
        let image = temporary_image_path("orphan-cleanup-failed-recovery");
        create_journaled_allocator_test_image(&mke2fs, &image);

        let bytes = fs::read(&image).expect("read generated recovery image");
        let device = Arc::new(LinuxImageDevice::new(bytes));
        let block_device: Arc<dyn BlockDevice> = device.clone();
        let mut filesystem =
            Ext4Filesystem::mount(block_device).expect("mount generated recovery image");
        let inode = allocate_checkpointed_directory_inode(&mut filesystem);
        let journal = filesystem.metadata_journal().expect("open mounted journal");
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        filesystem
            .set_orphan_head(Some(inode.number()), &mut handle)
            .unwrap();
        drop(handle);
        let commit = journal.force_commit_for_test(transaction).unwrap();
        filesystem
            .persist_metadata_journal_commit(&commit)
            .expect("persist orphan head update to journal");
        drop(filesystem);

        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::recover(recovery_device),
            Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind))
        );

        let bytes = device.bytes();
        let superblock = Superblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
            .expect("decode recovery-failed superblock");
        assert!(superblock.features().needs_recovery());
        assert_eq!(superblock.last_orphan(), inode.number().get());
        fs::remove_file(image).expect("remove recovery image");
    }

    #[test]
    fn recover_rejects_orphan_file_feature() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        set_allocator_feature_bits(
            &mut filesystem,
            features::CompatFeatures::ORPHAN_FILE,
            features::ReadOnlyCompatFeatures::empty(),
        );
        drop(filesystem);

        let recovery_device: Arc<dyn BlockDevice> = device.clone();
        assert_eq!(
            Ext4Filesystem::recover(recovery_device),
            Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile))
        );
    }

    #[test]
    fn inode_allocator_updates_directory_count_and_release_reverses_it() {
        let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(501));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let allocate_transaction = handle.id();

        let allocation = filesystem
            .allocate_inode_in_group(
                BlockGroupNumber::new(0),
                InodeInitialization::directory(0o755, 0, 0),
                &mut handle,
            )
            .unwrap();
        assert_eq!(filesystem.groups()[0].used_directories_count(), 1);
        assert_eq!(
            filesystem
                .internal_inode(allocation.inode())
                .unwrap()
                .kind(),
            InodeKind::Directory
        );
        drop(handle);

        let commit = journal.force_commit(allocate_transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let release_transaction = handle.id();
        let released = filesystem
            .release_allocated_inode(allocation.inode(), InodeKind::Directory, &mut handle)
            .unwrap();

        assert_eq!(released.inode(), InodeNumber::new(11));
        assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
        assert_eq!(filesystem.groups()[0].used_directories_count(), 0);
        assert_eq!(
            filesystem.superblock().free_inodes_count(),
            TEST_FREE_INODES
        );
        drop(handle);

        let commit = journal.force_commit(release_transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        let bytes = device.bytes();
        assert_eq!(bytes[3 * TEST_BLOCK_SIZE + 1], 0x03);
        assert_eq!(
            le_u16(&bytes, TEST_BLOCK_SIZE + 14),
            TEST_FREE_INODES as u16
        );
        assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 16), 0);
        assert_eq!(le_u32(&bytes, 1024 + 0x10), TEST_FREE_INODES);
        let inode_offset = 4 * TEST_BLOCK_SIZE + 10 * 256;
        assert!(
            bytes[inode_offset..inode_offset + 256]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn inode_allocator_rejects_releasing_reserved_inode_without_consuming_credits() {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let journal = JournalTransactions::new(TransactionId::new(601));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

        assert_eq!(
            filesystem.release_allocated_inode(
                InodeNumber::new(2),
                InodeKind::RegularFile,
                &mut handle
            ),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
        );
        assert_eq!(handle.remaining_credits(), 4);
    }

    #[test]
    fn journal_location_rejects_ambiguous_or_missing_journal_fields() {
        let inode = NonZeroU32::new(8);
        let device = NonZeroU32::new(1);

        assert_eq!(
            select_journal_location(false, inode, None, [0; 16]),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        );
        assert_eq!(
            select_journal_location(true, inode, device, [0; 16]),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        );
        assert_eq!(
            select_journal_location(true, None, None, [0; 16]),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        );
    }

    #[test]
    fn journal_location_preserves_external_device_and_uuid() {
        let device = NonZeroU32::new(7).unwrap();
        let uuid = [0x5a; 16];

        assert_eq!(
            select_journal_location(true, None, Some(device), uuid),
            Ok(JournalLocation::External { dev: device, uuid })
        );
    }

    #[test]
    fn journal_mapping_requires_every_block_to_be_written() {
        for invalid in [
            BlockMapping::Hole {
                len: BlockCount::new(1),
            },
            BlockMapping::Unwritten {
                physical: PhysicalBlock::new(100),
                len: BlockCount::new(1),
            },
            BlockMapping::Mapped {
                physical: PhysicalBlock::new(100),
                len: BlockCount::new(0),
            },
        ] {
            assert_eq!(
                collect_journal_extents(1, |_| Ok(invalid)),
                Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
            );
        }
    }

    #[test]
    fn journal_mapping_records_complete_mapped_ranges() {
        let extents = collect_journal_extents(4, |logical| match logical {
            0 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(100),
                len: BlockCount::new(2),
            }),
            2 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(200),
                len: BlockCount::new(4),
            }),
            _ => unreachable!(),
        })
        .unwrap();

        assert_eq!(
            extents,
            vec![
                JournalExtent {
                    logical_start: 0,
                    physical_start: 100,
                    len: 2,
                },
                JournalExtent {
                    logical_start: 2,
                    physical_start: 200,
                    len: 2,
                },
            ]
        );
    }

    #[test]
    fn mounted_journal_rejects_extent_beyond_filesystem_device() {
        let storage = InternalJournal {
            superblock: test_journal_superblock(900),
            extents: vec![JournalExtent {
                logical_start: 0,
                physical_start: 16,
                len: 1024,
            }],
            block_count: 1024,
        };

        assert!(matches!(
            MountedJournal::new(storage, 32),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        ));
    }

    #[test]
    fn mounted_journal_accepts_inode_capacity_larger_than_s_maxlen() {
        let storage = InternalJournal {
            superblock: test_journal_superblock(900),
            extents: vec![JournalExtent {
                logical_start: 0,
                physical_start: 16,
                len: 1025,
            }],
            block_count: 1024,
        };

        assert!(MountedJournal::new(storage, 2048).is_ok());
    }

    #[test]
    fn mounted_journal_rejects_mapping_shorter_than_s_maxlen() {
        let storage = InternalJournal {
            superblock: test_journal_superblock(900),
            extents: vec![JournalExtent {
                logical_start: 0,
                physical_start: 16,
                len: 1023,
            }],
            block_count: 1024,
        };

        assert!(matches!(
            MountedJournal::new(storage, 2048),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        ));
    }

    #[test]
    fn physical_block_validation_rejects_filesystem_metadata_zones() {
        let zones = [
            SystemZone {
                start: 20,
                end: 30,
                owner: None,
            },
            SystemZone {
                start: 40,
                end: 45,
                owner: Some(InodeNumber::new(8)),
            },
            SystemZone {
                start: 45,
                end: 50,
                owner: None,
            },
        ];

        assert!(!is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(12),
            20,
            1
        ));
        assert!(is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(12),
            30,
            1
        ));
        assert!(is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(8),
            40,
            5
        ));
        assert!(!is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(8),
            40,
            10
        ));
        assert!(!is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(12),
            0,
            1
        ));
        assert!(!is_inode_physical_block_valid(
            0,
            100,
            &zones,
            InodeNumber::new(12),
            99,
            2
        ));
    }

    fn allocator_test_filesystem(
        free_blocks: u32,
        bitmap_first_byte: u8,
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        allocator_test_filesystem_with_inodes(
            free_blocks,
            bitmap_first_byte,
            TEST_FREE_INODES,
            0x03,
            0,
        )
    }

    fn allocator_test_filesystem_with_inodes(
        free_blocks: u32,
        block_bitmap_first_byte: u8,
        free_inodes: u32,
        inode_bitmap_second_byte: u8,
        used_directories: u32,
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        allocator_test_filesystem_with_flags(
            free_blocks,
            block_bitmap_first_byte,
            free_inodes,
            inode_bitmap_second_byte,
            used_directories,
            0,
        )
    }

    fn allocator_test_filesystem_with_flags(
        free_blocks: u32,
        block_bitmap_first_byte: u8,
        free_inodes: u32,
        inode_bitmap_second_byte: u8,
        used_directories: u32,
        flags: u16,
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        allocator_test_filesystem_with_options(
            free_blocks,
            block_bitmap_first_byte,
            free_inodes,
            inode_bitmap_second_byte,
            used_directories,
            flags,
            false,
            false,
            false,
        )
    }

    fn allocator_test_filesystem_with_options(
        free_blocks: u32,
        block_bitmap_first_byte: u8,
        free_inodes: u32,
        inode_bitmap_second_byte: u8,
        used_directories: u32,
        flags: u16,
        metadata_checksum: bool,
        corrupt_block_bitmap_checksum: bool,
        corrupt_inode_bitmap_checksum: bool,
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        let mut image = vec![0; TEST_BLOCK_SIZE * TEST_BLOCK_COUNT];
        let mut superblock_bytes = allocator_superblock(free_blocks, free_inodes);
        if metadata_checksum {
            enable_allocator_metadata_checksum(&mut superblock_bytes);
        }
        image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);

        let mut descriptor =
            allocator_group_descriptor(free_blocks, free_inodes, used_directories, flags);
        write_allocator_bitmap(
            &mut image,
            2 * TEST_BLOCK_SIZE,
            &[block_bitmap_first_byte, 0, 0, 0],
            flags & TEST_EXT4_BG_BLOCK_UNINIT == 0,
        );
        write_allocator_bitmap(
            &mut image,
            3 * TEST_BLOCK_SIZE,
            &[0xff, inode_bitmap_second_byte, 0, 0],
            flags & TEST_EXT4_BG_INODE_UNINIT == 0,
        );

        let superblock = Superblock::decode(&superblock_bytes).unwrap();
        if metadata_checksum {
            update_allocator_descriptor_bitmap_checksums(
                &mut descriptor,
                0,
                superblock.checksum_seed(),
                &image[2 * TEST_BLOCK_SIZE..3 * TEST_BLOCK_SIZE],
                &image[3 * TEST_BLOCK_SIZE..4 * TEST_BLOCK_SIZE],
                corrupt_block_bitmap_checksum,
                corrupt_inode_bitmap_checksum,
            );
        }
        image[TEST_BLOCK_SIZE..TEST_BLOCK_SIZE + descriptor.len()].copy_from_slice(&descriptor);

        let block_device = Arc::new(TestDevice::new(image));
        let device: Arc<dyn BlockDevice> = block_device.clone();
        let filesystem_device = Arc::new(
            FilesystemDevice::open(device, TEST_BLOCK_SIZE, TEST_BLOCK_COUNT as u64).unwrap(),
        );
        let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
        let layout = FilesystemLayout::derive(&superblock).unwrap();
        let groups = vec![BlockGroupDescriptor::decode(&descriptor, true).unwrap()];

        let mut filesystem = Ext4Filesystem {
            device: filesystem_device,
            metadata_io,
            journal: None,
            superblock,
            layout,
            block_free_extent_caches: vec![None; groups.len()],
            groups,
            system_zones: Vec::new(),
        };
        filesystem.build_system_zones().unwrap();
        (filesystem, block_device)
    }

    fn journal_allocator_test_filesystem(
        free_blocks: u32,
        block_bitmap_first_byte: u8,
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        let mut image = vec![0; TEST_BLOCK_SIZE * TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT];
        let mut superblock_bytes = allocator_superblock_with_geometry(
            TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
            32,
            free_blocks,
            TEST_FREE_INODES,
        );
        put_u32(
            &mut superblock_bytes,
            0x20,
            TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
        );
        put_u32(
            &mut superblock_bytes,
            0x24,
            TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
        );
        image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);

        let descriptor = allocator_group_descriptor(free_blocks, TEST_FREE_INODES, 0, 0);
        image[TEST_BLOCK_SIZE..TEST_BLOCK_SIZE + descriptor.len()].copy_from_slice(&descriptor);

        let block_bitmap = &mut image[2 * TEST_BLOCK_SIZE..3 * TEST_BLOCK_SIZE];
        block_bitmap.fill(0xff);
        block_bitmap[0] = block_bitmap_first_byte;
        let mut remaining = free_blocks.saturating_sub(block_bitmap_first_byte.count_zeros());
        for block in 8..TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT {
            if (16..1040).contains(&block) || remaining == 0 {
                continue;
            }
            block_bitmap[block / 8] &= !(1 << (block % 8));
            remaining -= 1;
        }
        assert_eq!(remaining, 0);

        write_allocator_bitmap(&mut image, 3 * TEST_BLOCK_SIZE, &[0xff, 0x03, 0, 0], true);

        let block_device = Arc::new(TestDevice::new(image));
        let device: Arc<dyn BlockDevice> = block_device.clone();
        let filesystem_device = Arc::new(
            FilesystemDevice::open(
                device,
                TEST_BLOCK_SIZE,
                TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u64,
            )
            .unwrap(),
        );
        let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
        let superblock = Superblock::decode(&superblock_bytes).unwrap();
        let layout = FilesystemLayout::derive(&superblock).unwrap();
        let groups = vec![BlockGroupDescriptor::decode(&descriptor, true).unwrap()];
        let mut filesystem = Ext4Filesystem {
            device: filesystem_device,
            metadata_io,
            journal: None,
            superblock,
            layout,
            block_free_extent_caches: vec![None; groups.len()],
            groups,
            system_zones: Vec::new(),
        };
        filesystem.build_system_zones().unwrap();
        (filesystem, block_device)
    }

    fn allocator_multigroup_test_filesystem(
        groups: &[AllocatorGroupSpec],
    ) -> (Ext4Filesystem, Arc<TestDevice>) {
        let group_count = u32::try_from(groups.len()).unwrap();
        let block_count = group_count * TEST_BLOCK_COUNT as u32;
        let inodes_count = group_count * 32;
        let free_blocks = groups
            .iter()
            .map(|group| group.free_blocks)
            .try_fold(0u32, |sum, value| sum.checked_add(value))
            .unwrap();
        let free_inodes = groups
            .iter()
            .map(|group| group.free_inodes)
            .try_fold(0u32, |sum, value| sum.checked_add(value))
            .unwrap();
        let mut image = vec![0; usize::try_from(block_count).unwrap() * TEST_BLOCK_SIZE];
        let superblock_bytes =
            allocator_superblock_with_geometry(block_count, inodes_count, free_blocks, free_inodes);
        image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);

        let mut descriptors = Vec::new();
        for (index, group) in groups.iter().copied().enumerate() {
            let group = allocator_group_descriptor_for_group(u32::try_from(index).unwrap(), group);
            descriptors.push(group);
            let descriptor_start = TEST_BLOCK_SIZE + index * 64;
            image[descriptor_start..descriptor_start + 64].copy_from_slice(&group);

            let group_first = index * TEST_BLOCK_COUNT;
            let block_bitmap_start = (group_first + 2) * TEST_BLOCK_SIZE;
            write_allocator_bitmap(
                &mut image,
                block_bitmap_start,
                &groups[index].block_bitmap,
                groups[index].flags & TEST_EXT4_BG_BLOCK_UNINIT == 0,
            );
            let inode_bitmap_start = (group_first + 3) * TEST_BLOCK_SIZE;
            write_allocator_bitmap(
                &mut image,
                inode_bitmap_start,
                &groups[index].inode_bitmap,
                groups[index].flags & TEST_EXT4_BG_INODE_UNINIT == 0,
            );
        }

        let block_device = Arc::new(TestDevice::new(image));
        let device: Arc<dyn BlockDevice> = block_device.clone();
        let filesystem_device = Arc::new(
            FilesystemDevice::open(device, TEST_BLOCK_SIZE, u64::from(block_count)).unwrap(),
        );
        let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
        let superblock = Superblock::decode(&superblock_bytes).unwrap();
        let layout = FilesystemLayout::derive(&superblock).unwrap();
        let groups: Vec<BlockGroupDescriptor> = descriptors
            .iter()
            .map(|descriptor| BlockGroupDescriptor::decode(descriptor, true).unwrap())
            .collect();

        let mut filesystem = Ext4Filesystem {
            device: filesystem_device,
            metadata_io,
            journal: None,
            superblock,
            layout,
            block_free_extent_caches: vec![None; groups.len()],
            groups,
            system_zones: Vec::new(),
        };
        filesystem.build_system_zones().unwrap();
        (filesystem, block_device)
    }

    fn allocate_contiguous_blocks(
        filesystem: &mut Ext4Filesystem,
        count: u32,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> PhysicalBlock {
        let mut first = None;
        for index in 0..count {
            let allocation = filesystem.allocate_block(None, handle).unwrap();
            if index == 0 {
                first = Some(allocation.block());
            } else {
                assert_eq!(
                    allocation.block(),
                    PhysicalBlock::new(first.unwrap().get() + u64::from(index))
                );
            }
        }
        first.unwrap()
    }

    fn allocator_superblock(
        free_blocks: u32,
        free_inodes: u32,
    ) -> [u8; superblock::SUPERBLOCK_SIZE] {
        allocator_superblock_with_geometry(TEST_BLOCK_COUNT as u32, 32, free_blocks, free_inodes)
    }

    fn allocate_checkpointed_regular_inode(filesystem: &mut Ext4Filesystem) -> crate::Ext4Inode {
        allocate_checkpointed_inode(filesystem, InodeInitialization::regular_file(0o644, 0, 0))
    }

    fn allocate_checkpointed_directory_inode(filesystem: &mut Ext4Filesystem) -> crate::Ext4Inode {
        allocate_checkpointed_inode(filesystem, InodeInitialization::directory(0o755, 0, 0))
    }

    fn allocate_checkpointed_inode(
        filesystem: &mut Ext4Filesystem,
        initialization: InodeInitialization,
    ) -> crate::Ext4Inode {
        let journal = JournalTransactions::new(TransactionId::new(690));
        let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
        let transaction = handle.id();
        let allocation = filesystem
            .allocate_inode(None, initialization, &mut handle)
            .unwrap();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();
        filesystem.internal_inode(allocation.inode()).unwrap()
    }

    fn install_test_internal_journal(filesystem: &mut Ext4Filesystem, sequence: u32) {
        let journal_block = FilesystemBlock::new(16);
        let journal_blocks = 1024;
        assert!(journal_block.get() + u64::from(journal_blocks) <= filesystem.layout.block_count);
        if !filesystem.is_system_zone_block(journal_block) {
            filesystem
                .add_system_zone(journal_block.get(), u64::from(journal_blocks), None)
                .unwrap();
        }
        let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
        let end = offset + len;
        let mut bytes = vec![0; filesystem.device.block_size()];
        filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
        let superblock_bytes = bytes.get_mut(offset..end).unwrap();
        let compat = le_u32(superblock_bytes, 0x5c) | features::CompatFeatures::HAS_JOURNAL.bits();
        put_u32(superblock_bytes, 0x5c, compat);
        filesystem.superblock = Superblock::decode(superblock_bytes).unwrap();
        filesystem
            .device
            .write_contiguous_blocks(block, 1, &bytes)
            .unwrap();
        let journal_superblock_bytes = test_journal_superblock_bytes(sequence);
        let mut journal_block_bytes = vec![0; TEST_BLOCK_SIZE];
        journal_block_bytes[..journal_superblock_bytes.len()]
            .copy_from_slice(&journal_superblock_bytes);
        filesystem
            .write_contiguous_blocks(journal_block, 1, &journal_block_bytes)
            .unwrap();
        filesystem.journal = Some(
            MountedJournal::new(
                InternalJournal {
                    superblock: test_journal_superblock(sequence),
                    extents: vec![JournalExtent {
                        logical_start: 0,
                        physical_start: journal_block.get(),
                        len: journal_blocks,
                    }],
                    block_count: journal_blocks,
                },
                filesystem.layout.block_count,
            )
            .unwrap(),
        );
    }

    fn persist_test_orphan_head(
        filesystem: &mut Ext4Filesystem,
        head: Option<InodeNumber>,
        sequence: u32,
    ) {
        let journal = JournalTransactions::new(TransactionId::new(sequence));
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        filesystem.set_orphan_head(head, &mut handle).unwrap();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();
    }

    fn update_test_inode_links_count(
        filesystem: &mut Ext4Filesystem,
        inode: &Ext4Inode,
        links_count: u16,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Inode {
        let inode_table_block = filesystem.inode_table_entry_block(inode.number()).unwrap();
        let inode_table_access = filesystem
            .metadata_io
            .write_access(inode_table_block, handle)
            .unwrap();
        let mut inode_table_bytes = metadata_access_bytes(&inode_table_access).unwrap();
        let updated = filesystem
            .update_inode_table_entry_allow_zero_links(
                &mut inode_table_bytes,
                inode.number(),
                |inode_bytes| {
                    put_u16(inode_bytes, 0x1a, links_count);
                    Ok(())
                },
            )
            .unwrap();
        replace_metadata_access_bytes(&inode_table_access, inode_table_bytes).unwrap();
        updated
    }

    fn set_allocator_feature_bits(
        filesystem: &mut Ext4Filesystem,
        compat_features: features::CompatFeatures,
        read_only_compat_features: features::ReadOnlyCompatFeatures,
    ) {
        let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
        let end = offset + len;
        let mut bytes = vec![0; filesystem.device.block_size()];
        filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
        let superblock_bytes = bytes.get_mut(offset..end).unwrap();
        let compat = le_u32(superblock_bytes, 0x5c) | compat_features.bits();
        let ro_compat = le_u32(superblock_bytes, 0x64) | read_only_compat_features.bits();
        put_u32(superblock_bytes, 0x5c, compat);
        put_u32(superblock_bytes, 0x64, ro_compat);
        filesystem.superblock = Superblock::decode(superblock_bytes).unwrap();
        filesystem
            .device
            .write_contiguous_blocks(block, 1, &bytes)
            .unwrap();
    }

    fn enable_allocator_flex_bg(filesystem: &mut Ext4Filesystem, log_groups_per_flex: u8) {
        let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
        let end = offset + len;
        let mut bytes = vec![0; filesystem.device.block_size()];
        filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
        let superblock_bytes = bytes.get_mut(offset..end).unwrap();
        let incompat = le_u32(superblock_bytes, 0x60) | features::IncompatFeatures::FLEX_BG.bits();
        put_u32(superblock_bytes, 0x60, incompat);
        superblock_bytes[0x174] = log_groups_per_flex;
        filesystem.superblock = Superblock::decode(superblock_bytes).unwrap();
        filesystem.layout = FilesystemLayout::derive(&filesystem.superblock).unwrap();
        filesystem
            .device
            .write_contiguous_blocks(block, 1, &bytes)
            .unwrap();
    }

    fn test_journal_superblock(sequence: u32) -> JournalSuperblock {
        let bytes = test_journal_superblock_bytes(sequence);
        JournalSuperblock::decode(&bytes, TEST_BLOCK_SIZE as u32, 1024, [0; 16]).unwrap()
    }

    fn test_journal_superblock_bytes(sequence: u32) -> [u8; 1024] {
        let mut bytes = [0; 1024];
        put_be_u32(&mut bytes, 0x00, 0xc03b_3998);
        put_be_u32(&mut bytes, 0x04, 3);
        put_be_u32(&mut bytes, 0x0c, TEST_BLOCK_SIZE as u32);
        put_be_u32(&mut bytes, 0x10, 1024);
        put_be_u32(&mut bytes, 0x14, 1);
        put_be_u32(&mut bytes, 0x18, sequence);
        put_be_u32(&mut bytes, 0x1c, 0);
        put_be_u32(&mut bytes, 0x58, 1);
        bytes
    }

    fn allocator_superblock_with_geometry(
        block_count: u32,
        inodes_count: u32,
        free_blocks: u32,
        free_inodes: u32,
    ) -> [u8; superblock::SUPERBLOCK_SIZE] {
        let mut bytes = [0; superblock::SUPERBLOCK_SIZE];
        put_u32(&mut bytes, 0x00, inodes_count);
        put_u32(&mut bytes, 0x04, block_count);
        put_u32(&mut bytes, 0x0c, free_blocks);
        put_u32(&mut bytes, 0x10, free_inodes);
        put_u32(&mut bytes, 0x14, 0);
        put_u32(&mut bytes, 0x18, 2);
        put_u32(&mut bytes, 0x1c, 2);
        put_u32(&mut bytes, 0x20, TEST_BLOCK_COUNT as u32);
        put_u32(&mut bytes, 0x24, TEST_BLOCK_COUNT as u32);
        put_u32(&mut bytes, 0x28, 32);
        put_u16(&mut bytes, 0x38, 0xef53);
        put_u32(&mut bytes, 0x4c, 1);
        put_u32(&mut bytes, 0x54, 11);
        put_u16(&mut bytes, 0x58, 256);
        put_u32(
            &mut bytes,
            0x60,
            features::IncompatFeatures::EXTENTS
                .union(features::IncompatFeatures::BIT_64)
                .bits(),
        );
        put_u16(&mut bytes, 0xfe, 64);
        bytes
    }

    fn enable_allocator_metadata_checksum(bytes: &mut [u8]) {
        put_u32(
            bytes,
            0x64,
            features::ReadOnlyCompatFeatures::METADATA_CSUM.bits(),
        );
        bytes[0x175] = 1;
        update_allocator_superblock_checksum(bytes);
    }

    fn update_allocator_superblock_checksum(bytes: &mut [u8]) {
        let checksum = checksum::crc32c(u32::MAX, &bytes[..0x3fc]);
        put_u32(bytes, 0x3fc, checksum);
    }

    fn update_allocator_descriptor_bitmap_checksums(
        descriptor: &mut [u8],
        group: u32,
        checksum_seed: u32,
        block_bitmap: &[u8],
        inode_bitmap: &[u8],
        corrupt_block_bitmap_checksum: bool,
        corrupt_inode_bitmap_checksum: bool,
    ) {
        let block_checksum = maybe_corrupt_checksum(
            checksum::bitmap_checksum(&block_bitmap[..TEST_BLOCK_COUNT / 8], checksum_seed),
            corrupt_block_bitmap_checksum,
        );
        let inode_checksum = maybe_corrupt_checksum(
            checksum::bitmap_checksum(&inode_bitmap[..TEST_BLOCK_COUNT / 8], checksum_seed),
            corrupt_inode_bitmap_checksum,
        );

        put_u16(descriptor, 24, block_checksum as u16);
        put_u16(descriptor, 26, inode_checksum as u16);
        put_u16(descriptor, 56, (block_checksum >> 16) as u16);
        put_u16(descriptor, 58, (inode_checksum >> 16) as u16);
        put_u16(descriptor, 30, 0);
        let descriptor_checksum =
            checksum::group_descriptor_checksum(descriptor, group, checksum_seed).unwrap();
        put_u16(descriptor, 30, descriptor_checksum);
    }

    fn maybe_corrupt_checksum(checksum: u32, corrupt: bool) -> u32 {
        if corrupt { checksum ^ 1 } else { checksum }
    }

    fn allocator_group_descriptor_for_group(group: u32, spec: AllocatorGroupSpec) -> [u8; 64] {
        let group_first = group * TEST_BLOCK_COUNT as u32;
        let mut bytes = [0; 64];
        put_u32(&mut bytes, 0, group_first + 2);
        put_u32(&mut bytes, 4, group_first + 3);
        put_u32(&mut bytes, 8, group_first + 4);
        put_u16(&mut bytes, 12, spec.free_blocks as u16);
        put_u16(&mut bytes, 14, spec.free_inodes as u16);
        put_u16(&mut bytes, 16, spec.used_directories as u16);
        put_u16(&mut bytes, 18, spec.flags);
        bytes
    }

    fn allocator_group_descriptor(
        free_blocks: u32,
        free_inodes: u32,
        used_directories: u32,
        flags: u16,
    ) -> [u8; 64] {
        let mut bytes = [0; 64];
        put_u32(&mut bytes, 0, 2);
        put_u32(&mut bytes, 4, 3);
        put_u32(&mut bytes, 8, 4);
        put_u16(&mut bytes, 12, free_blocks as u16);
        put_u16(&mut bytes, 14, free_inodes as u16);
        put_u16(&mut bytes, 16, used_directories as u16);
        put_u16(&mut bytes, 18, flags);
        bytes
    }

    fn write_allocator_bitmap(image: &mut [u8], offset: usize, prefix: &[u8], initialized: bool) {
        let bitmap = &mut image[offset..offset + TEST_BLOCK_SIZE];
        bitmap[..prefix.len()].copy_from_slice(prefix);
        if initialized {
            bitmap[prefix.len()..].fill(0xff);
        }
    }

    fn block_start(block_id: u64) -> Result<usize, DriverError> {
        usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(TEST_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)
    }

    fn linux_image_device_block_start(block_id: u64) -> Result<usize, DriverError> {
        usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(LINUX_IMAGE_DEVICE_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)
    }

    fn create_allocator_test_image(mke2fs: &Path, image: &Path) {
        create_allocator_image_with_features(
            mke2fs,
            image,
            "extent,filetype,64bit,flex_bg,metadata_csum,dir_index,^has_journal,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp,^huge_file",
        )
    }

    fn create_journaled_allocator_test_image(mke2fs: &Path, image: &Path) {
        create_allocator_image_with_features(
            mke2fs,
            image,
            "extent,filetype,64bit,flex_bg,metadata_csum,dir_index,has_journal,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp,^huge_file",
        )
    }

    fn create_journaled_linear_namespace_test_image(mke2fs: &Path, image: &Path) {
        create_allocator_image_with_features(
            mke2fs,
            image,
            "extent,filetype,64bit,flex_bg,metadata_csum,has_journal,^dir_index,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp,^huge_file",
        )
    }

    fn create_journaled_huge_file_namespace_test_image(mke2fs: &Path, image: &Path) {
        create_allocator_image_with_features(
            mke2fs,
            image,
            "extent,filetype,64bit,flex_bg,metadata_csum,has_journal,^dir_index,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp,huge_file",
        )
    }

    fn create_journaled_indexed_namespace_test_image(mke2fs: &Path, image: &Path) {
        create_allocator_image_with_features(
            mke2fs,
            image,
            "extent,filetype,64bit,flex_bg,metadata_csum,has_journal,dir_index,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp,^huge_file",
        )
    }

    fn create_allocator_image_with_features(mke2fs: &Path, image: &Path, features: &str) {
        let file = fs::File::create(image).expect("create allocator ext4 image");
        file.set_len(256 * 1024 * 1024)
            .expect("size allocator ext4 image");
        let status = Command::new(mke2fs)
            .args(["-q", "-t", "ext4", "-F", "-b", "4096", "-I", "256"])
            .arg("-O")
            .arg(features)
            .arg(image)
            .status()
            .expect("run mke2fs for allocator image");
        assert!(status.success(), "mke2fs allocator image failed");
    }

    fn run_e2fsck_read_only(e2fsck: &Path, image: &Path) {
        let status = Command::new(e2fsck)
            .args(["-f", "-n"])
            .arg(image)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run e2fsck for allocator image");
        assert_eq!(status.code(), Some(0), "e2fsck rejected allocator image");
    }

    fn run_e2fsck_rebuild_index(e2fsck: &Path, image: &Path) {
        let status = Command::new(e2fsck)
            .args(["-f", "-y", "-D"])
            .arg(image)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run e2fsck -D for indexed namespace image");
        assert!(
            status.success(),
            "e2fsck -D rejected indexed namespace image"
        );
    }

    fn run_debugfs(debugfs: &Path, image: &Path, command: &str) {
        let status = Command::new(debugfs)
            .args(["-w", "-R", command])
            .arg(image)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run debugfs for allocator image");
        assert!(status.success(), "debugfs command failed: {command}");
    }

    fn require_e2fsprogs(name: &str) -> PathBuf {
        find_e2fsprogs(name).unwrap_or_else(|| {
            panic!("{name} is required for kext4 allocator interoperability tests")
        })
    }

    fn find_e2fsprogs(name: &str) -> Option<PathBuf> {
        [
            PathBuf::from(name),
            PathBuf::from("/opt/homebrew/opt/e2fsprogs/sbin").join(name),
            PathBuf::from("/usr/local/opt/e2fsprogs/sbin").join(name),
        ]
        .into_iter()
        .find(|path| Command::new(path).arg("-V").output().is_ok())
    }

    fn temporary_image_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kext4-{label}-{}.img", std::process::id()))
    }

    fn le_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn le_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn put_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_be_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// Builds an unlinked (links_count == 0) regular-file inode carrying the
    /// given extents, with an internal journal installed, ready for the
    /// three-phase eviction protocol.  Each extent is allocated contiguously.
    fn eviction_test_unlinked_inode(
        extents: &[(u64, u32)],
    ) -> (Ext4Filesystem, crate::InodeNumber, crate::Ext4Timestamp) {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        install_test_internal_journal(&mut filesystem, 2000);
        let journal = JournalTransactions::new(TransactionId::new(2000));
        let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
        let transaction = handle.id();

        let inode_allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::regular_file(0o644, 0, 0),
                &mut handle,
            )
            .unwrap();
        let mut inode = filesystem.internal_inode(inode_allocation.inode()).unwrap();

        for (logical, len) in extents {
            let physical = allocate_contiguous_blocks(&mut filesystem, *len, &mut handle);
            inode = filesystem
                .insert_extent_mapping(
                    &inode,
                    LogicalBlock::new(*logical),
                    physical,
                    BlockCount::new(*len),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
        }

        let timestamp = crate::Ext4Timestamp::new(2000, 0);
        inode = filesystem
            .update_inode_links_count_ctime_metadata(&inode, 0, timestamp, &mut handle)
            .unwrap();

        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        filesystem
            .metadata_io
            .checkpoint_committed(&commit)
            .unwrap();
        journal.finish_checkpoint_for_test(&commit).unwrap();

        // Reset the internal journal so the eviction protocol begins from a
        // clean journal with a fresh sequence, matching the installed
        // superblock (mirrors the per-phase reinstall used elsewhere).
        install_test_internal_journal(&mut filesystem, 2001);

        (filesystem, inode_allocation.inode(), timestamp)
    }

    #[test]
    fn eviction_release_batch_partial_extent_split() {
        let (mut filesystem, number, timestamp) = eviction_test_unlinked_inode(&[(0, 100)]);
        let mut evict = filesystem.eviction_prepare(number, timestamp).unwrap();

        // max_blocks = 40 < 100, so only the tail 40 blocks are released.
        let (freed, done) = filesystem.eviction_release_batch(&mut evict, 40).unwrap();
        assert_eq!(freed, 40);
        assert!(!done);

        // The remaining extent must keep logical [0, 60).
        let inode = filesystem.referenced_inode(number).unwrap();
        let collected = filesystem.collect_extent_tree(&inode).unwrap();
        assert_eq!(collected.extents.len(), 1);
        assert_eq!(collected.extents[0].logical, 0);
        assert_eq!(collected.extents[0].len, 60);

        // Drain the rest under a bounded loop.
        let mut batches = 1;
        loop {
            let (_, done) = filesystem.eviction_release_batch(&mut evict, 40).unwrap();
            batches += 1;
            assert!(batches <= 10, "eviction must terminate");
            if done {
                break;
            }
        }

        let inode = filesystem.referenced_inode(number).unwrap();
        let collected = filesystem.collect_extent_tree(&inode).unwrap();
        assert!(collected.extents.is_empty());
        filesystem.eviction_finish(evict).unwrap();
    }

    #[test]
    fn eviction_release_batch_single_batch_empties_tree() {
        // 3 extents of 10 blocks = 30 blocks total, well within max_blocks.
        let (mut filesystem, number, timestamp) =
            eviction_test_unlinked_inode(&[(0, 10), (10, 10), (20, 10)]);
        let mut evict = filesystem.eviction_prepare(number, timestamp).unwrap();

        // All extents fit in one batch, so the tree is emptied and done = true.
        let (freed, done) = filesystem.eviction_release_batch(&mut evict, 256).unwrap();
        assert_eq!(freed, 30);
        assert!(done);

        let inode = filesystem.referenced_inode(number).unwrap();
        let collected = filesystem.collect_extent_tree(&inode).unwrap();
        assert!(collected.extents.is_empty());
        filesystem.eviction_finish(evict).unwrap();
    }

    #[test]
    fn eviction_release_batch_multi_batch_drains_tree() {
        // 10 extents of 10 blocks = 100 blocks, released in max_blocks = 30
        // batches, exercising the keep_count != 0 / keep_count != extents.len()
        // branch across multiple iterations.
        let extents: [(u64, u32); 10] = [
            (0, 10),
            (10, 10),
            (20, 10),
            (30, 10),
            (40, 10),
            (50, 10),
            (60, 10),
            (70, 10),
            (80, 10),
            (90, 10),
        ];
        let (mut filesystem, number, timestamp) = eviction_test_unlinked_inode(&extents);
        let mut evict = filesystem.eviction_prepare(number, timestamp).unwrap();

        let mut batches = 0;
        loop {
            let (_, done) = filesystem.eviction_release_batch(&mut evict, 30).unwrap();
            batches += 1;
            assert!(
                batches <= 10,
                "eviction must terminate within bounded batches"
            );
            if done {
                break;
            }
        }
        assert!(
            batches >= 2,
            "expected more than one batch for 100 blocks at 30/batch"
        );

        let inode = filesystem.referenced_inode(number).unwrap();
        let collected = filesystem.collect_extent_tree(&inode).unwrap();
        assert!(collected.extents.is_empty());
        filesystem.eviction_finish(evict).unwrap();
    }

    #[test]
    fn eviction_release_batch_no_data_blocks_fast_path() {
        // No extents inserted: an unlinked inode with zero data blocks must
        // take the fast path and report (0, true) without touching the tree.
        let (mut filesystem, number, timestamp) = eviction_test_unlinked_inode(&[]);
        let mut evict = filesystem.eviction_prepare(number, timestamp).unwrap();

        let (freed, done) = filesystem.eviction_release_batch(&mut evict, 256).unwrap();
        assert_eq!(freed, 0);
        assert!(done);

        filesystem.eviction_finish(evict).unwrap();
    }
}

fn validate_group(
    superblock: &Superblock,
    layout: &FilesystemLayout,
    group: u32,
    descriptor: &BlockGroupDescriptor,
) -> Ext4Result<()> {
    let first = u64::from(superblock.first_data_block())
        .checked_add(
            u64::from(group)
                .checked_mul(u64::from(superblock.blocks_per_group()))
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    let end = first
        .checked_add(u64::from(superblock.blocks_per_group()))
        .ok_or(Ext4Error::Overflow)?
        .min(superblock.blocks_count());

    let in_group = |block: u64| block >= first && block < end;
    let inode_table_end = descriptor
        .inode_table()
        .checked_add(u64::from(layout.inode_table_blocks_per_group))
        .ok_or(Ext4Error::Overflow)?;
    let is_valid = if superblock.features().has_flex_bg() {
        let groups_per_flex = 1u32
            .checked_shl(u32::from(superblock.log_groups_per_flex()))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidFlexGeometry))?;
        let flex_first_group = group / groups_per_flex * groups_per_flex;
        let flex_first = u64::from(superblock.first_data_block())
            .checked_add(
                u64::from(flex_first_group)
                    .checked_mul(u64::from(superblock.blocks_per_group()))
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let flex_group_count = groups_per_flex.min(layout.group_count - flex_first_group);
        let flex_end = flex_first
            .checked_add(
                u64::from(flex_group_count)
                    .checked_mul(u64::from(superblock.blocks_per_group()))
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?
            .min(superblock.blocks_count());
        let in_flex = |block: u64| block >= flex_first && block < flex_end;

        in_flex(descriptor.block_bitmap())
            && in_flex(descriptor.inode_bitmap())
            && descriptor.inode_table() >= flex_first
            && inode_table_end <= flex_end
    } else {
        in_group(descriptor.block_bitmap())
            && in_group(descriptor.inode_bitmap())
            && descriptor.inode_table() >= first
            && inode_table_end <= end
    };
    if !is_valid {
        return Err(Ext4Error::Corrupt(CorruptKind::MetadataOutsideGroup));
    }
    Ok(())
}
