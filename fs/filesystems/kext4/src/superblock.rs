// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::num::NonZeroU32;

use block::BlockDeviceOperations;
use ksync::{Mutex, MutexGuard};
use ktime_types::TimestampLimits;

use crate::{
    bitmap_allocator::{BlockGroupRange, InodeGroupRange},
    buffer::{Ext4MetadataIo, MetadataBuffer, MetadataWriteAccess},
    dirhash::DX_HASH_UNSIGNED_OFFSET,
    disk::{
        BlockGroupDescriptor, Ext4DiskSuperblock, GroupGeometry, GroupMutableState, checksum,
        dir::DX_HASH_SIPHASH,
        features, superblock,
        superblock::{EXT2_FLAGS_SIGNED_HASH, EXT2_FLAGS_UNSIGNED_HASH},
    },
    error::{ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, UnsupportedKind},
    extent::BlockMapping,
    inode::{InodeKind, timestamp_limits_for_inode_size},
    io::FilesystemDevice,
    jbd2::{
        JournalBlock, JournalBlockMapper, JournalLogScan, JournalReplayApplied,
        JournalReplayReport, JournalStart, JournalSuperblock, replay_scanned_journal, scan_journal,
    },
    journal::MountedJournal,
    mballoc::BlockGroupFreeExtentCache,
    orphan::OrphanInfo,
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

/// Selects the ext4 accounting convention for `statfs().f_blocks`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Ext4StatFsMode {
    /// Subtract filesystem metadata overhead from the reported total.
    #[default]
    Bsd,
    /// Report the complete on-disk filesystem block count.
    Minix,
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
    fn derive(superblock: &Ext4DiskSuperblock) -> Ext4Result<Self> {
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

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

/// Mutable superblock recovery and block/inode accounting state guarded by
/// the allocator lock.
///
/// Linux keeps this state in `ext4_sb_info` under per-group spinlocks and
/// percpu counters; KExt4 currently concentrates it in one sleepable mutex.
/// Balloc/ialloc entries hold the lock across their whole read-modify-write
/// (snapshot, bitmap edits, journal slots, in-memory publish) so concurrent
/// callers cannot double-allocate or lose counter updates. The multi-group
/// scan entries (`allocate_blocks_for_write`, `allocate_inode_with_name`)
/// are not single critical sections: the scan peeks each group's hint under
/// a short lock, then re-acquires the lock inside each `*_in_group` attempt.
/// The mount-wide delayed-allocation total is deliberately kept out of this
/// lock: it lives behind the separate
/// [`Ext4SbInfo::delalloc_reserved_blocks`] mutex, so reserve/release only
/// serialize against other delalloc mutations, not against block-group
/// allocation (Linux `s_dirtyclusters_counter` analog, updated by atomic
/// percpu counters there; a dedicated mutex is KExt4's smaller-footprint
/// equivalent). Admission checks additionally read this lock's free-block
/// aggregate; the read order is fixed to this lock first, then the delalloc
/// mutex, and no path takes them the other way around.
/// Holders may call into `metadata_io` and the journal while holding the
/// lock, but must never re-enter anything that takes it again: public
/// allocator entries as well as the lock-taking accessors (`groups()`,
/// `free_blocks_count()`, `free_inodes_count()`, `needs_recovery()`,
/// `has_orphan_present()`) — use
/// the in-guard fields, the frozen `group_geometry` table, or the
/// `*_in_group` lock-free variants instead (the lock is not recursive).
/// Entries pre-warm the metadata cache
/// ([`Ext4SbInfo::prefetch_metadata_blocks`])
/// before taking this lock so cold-cache device reads run outside the
/// critical section; only the inode-table block whose address depends on
/// the allocation decision is still read in-lock.
/// Live counters and mutable recovery flags travel here. Static feature bits
/// and primary-superblock geometry stay in the frozen
/// [`Ext4SbInfo::superblock`] mirror, and
/// per-group addresses (block/inode bitmap, inode-table start) in the frozen
/// [`Ext4SbInfo::group_geometry`] table, so lock-free readers never see
/// allocator churn and never lock just to read static geometry (matching
/// Linux `ext4_get_group_desc(..., NULL)`). The `groups` Vec keeps only the
/// mutable per-group descriptor fields ([`GroupMutableState`]) for the
/// allocator read-modify-write; address fields are deliberately absent, so
/// the frozen geometry table is the single read source for addresses and
/// [`Ext4SbInfo::reload_mutable_metadata_state`] rejects replayed
/// descriptors whose addresses changed instead of splitting the two states.
pub(crate) struct AllocatorState {
    pub(crate) free_blocks_count: u64,
    pub(crate) free_inodes_count: u32,
    /// Mount-wide directory-inode total (Linux `s_dirs_counter` analog).
    ///
    /// Initialized at mount from a one-time fold over the decoded group
    /// descriptors and then updated at the same sites as the per-group
    /// `bg_used_dirs_count`, so it stays exact under the single allocator
    /// mutex (where Linux's percpu counter is an approximation).
    /// `inode_allocation_totals` reads it in constant time instead of
    /// folding every group under this lock.
    pub(crate) used_directories_count: u32,
    pub(crate) last_orphan: u32,
    pub(crate) needs_recovery: bool,
    pub(crate) has_orphan_present: bool,
    pub(crate) groups: Vec<GroupMutableState>,
    pub(crate) block_free_extent_caches: Vec<Option<BlockGroupFreeExtentCache>>,
}

/// A validated ext4 filesystem mount core.
///
/// KVFS owns resident inode identity. This object owns ext4 superblock state,
/// including the mount-wide delayed-allocation reservation aggregate.
pub(crate) struct Ext4SbInfo {
    pub(crate) allocator: Mutex<AllocatorState>,
    /// Mount-wide delayed-allocation reservation total.
    ///
    /// A dedicated lock keeps the reserve/release/truncate/eviction paths
    /// from serializing against block-group allocation on the allocator
    /// mutex, mirroring Linux where `s_dirtyclusters_counter` is an atomic
    /// per-CPU counter updated outside the group locks. Admission checks
    /// additionally read the allocator's free-block aggregate; the read
    /// order is fixed to allocator lock first, then this lock.
    pub(crate) delalloc_reserved_blocks: Mutex<u64>,
    /// Mount-time mirror of the decoded primary superblock.
    ///
    /// Geometry and static feature bits never change after mount; live
    /// free-block/free-inode counters, the orphan-list head, `RECOVER`, and
    /// `ORPHAN_PRESENT` live in [`AllocatorState`] and are read through
    /// dedicated accessors.
    pub(crate) superblock: Ext4DiskSuperblock,
    /// Per-group geometry (block/inode bitmap and inode-table addresses)
    /// frozen at mount.
    ///
    /// This table is the mount's single read source for these addresses;
    /// [`AllocatorState`] carries no address fields. The inode-table hot
    /// path reads the table without the allocator lock, mirroring Linux
    /// where `ext4_get_group_desc(..., NULL)` reads the same fields from
    /// the page-cached descriptor table lock-free. Journal replay may
    /// rewrite descriptor blocks, so
    /// [`Self::reload_mutable_metadata_state`] re-validates the replayed
    /// descriptors and fails closed on any address change instead of
    /// letting the reloaded mutable state diverge from this table. Live
    /// counters and flags stay in [`AllocatorState`] under the allocator
    /// lock.
    group_geometry: Vec<GroupGeometry>,
    statfs_mode: Ext4StatFsMode,
    /// Maximum byte size supported by legacy indirect-block inodes.
    pub(crate) bitmap_maxbytes: u64,
    // Linux `ext4_sb_info::s_hash_unsigned`: zero for signed algorithms and
    // three for the corresponding unsigned HTree algorithms.
    pub(crate) hash_unsigned: u8,
    pub(crate) device: Arc<FilesystemDevice>,
    pub(crate) metadata_io: Ext4MetadataIo,
    pub(crate) journal: Option<Arc<MountedJournal>>,
    pub(crate) orphan_info: Option<OrphanInfo>,
    pub(crate) layout: FilesystemLayout,
    // Read-only system zones scanned from the on-disk bitmap at mount time.
    // Zones are never removed and the release path rejects every system-zone
    // block unconditionally, matching Linux where a system zone is never
    // freed. The owner exception is limited to block-reference validation:
    // `is_inode_physical_block_valid` lets the journal inode map blocks inside
    // its own protected zone, but such blocks can never be released.
    system_zones: Vec<SystemZone>,
}

pub(crate) struct Ext4Recovery {
    pub(crate) filesystem: Ext4SbInfo,
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

impl Ext4SbInfo {
    /// Builds test mount state and commits writable mount-finalization metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Ext4Error::Unsupported`] when the filesystem does not enable
    /// extent-based block mapping. The mounted core exposes mutation APIs, so
    /// it does not publish a filesystem whose legacy indirect block maps can
    /// only be read but not safely allocated, truncated, or freed.
    #[cfg(unittest)]
    pub(crate) fn mount(device: Arc<dyn BlockDeviceOperations>) -> Ext4Result<Self> {
        let mut filesystem = Self::prepare_mount_with_statfs_mode(device, Ext4StatFsMode::Bsd)?;
        filesystem.persist_directory_hash_policy()?;
        Ok(filesystem)
    }

    /// Prepares ext4 mount state without committing mount-success metadata.
    pub(crate) fn prepare_mount_with_statfs_mode(
        device: Arc<dyn BlockDeviceOperations>,
        statfs_mode: Ext4StatFsMode,
    ) -> Ext4Result<Self> {
        let mut filesystem = Self::open(device, false)?;
        filesystem.statfs_mode = statfs_mode;
        Ok(filesystem)
    }

    pub(crate) fn set_statfs_mode(&mut self, statfs_mode: Ext4StatFsMode) {
        self.statfs_mode = statfs_mode;
    }

    /// Recovers a filesystem journal and cleans ext4 orphan metadata.
    ///
    /// This entry point performs journal replay and orphan metadata writes. It
    /// is intentionally separate from [`mount`](Self::mount); the writable
    /// mount path may persist Linux's default directory-hash signedness, but it
    /// does not implicitly perform recovery. A filesystem can have a clean
    /// journal but still contain legacy-list or orphan-file entries left by a
    /// committed unlink or truncate. In that case this method performs
    /// journaled orphan cleanup and returns `Ok(None)` because no journal replay
    /// report was produced.
    ///
    /// A successful return means that orphan cleanup has completed its journal
    /// commit and home-block checkpoint, the journal is marked empty, and the
    /// ext4 recovery features have been cleared on stable storage. On failure,
    /// the method does not intentionally clear the final on-disk recovery
    /// evidence before the corresponding journal or orphan work is durable.
    /// A reserved, out-of-range, or unallocated legacy orphan head, a linked
    /// inode kind that ext4 cannot truncate, and an invalid next pointer all
    /// terminate the damaged legacy chain through a journaled zero-head update.
    /// Invalid orphan-file entries, bitmap I/O/checksum failures, inode decode
    /// failures, and unsupported but structurally valid cleanup work remain
    /// caller-visible errors.
    ///
    /// # Errors
    ///
    /// Returns [`Ext4Error::Unsupported`] before replay when the filesystem
    /// lacks extent-based block mapping, because recovery may need the same
    /// mutation and block-release support as a normal writable mount.
    pub(crate) fn recover(
        device: Arc<dyn BlockDeviceOperations>,
    ) -> Ext4Result<Option<Ext4RecoveryReport>> {
        Ext4Recovery::open(device)?.replay()
    }

    pub(crate) fn open(
        device: Arc<dyn BlockDeviceOperations>,
        allow_recovery: bool,
    ) -> Ext4Result<Self> {
        let mut superblock_bytes = [0; superblock::SUPERBLOCK_SIZE];
        FilesystemDevice::read_bytes(
            device.as_ref(),
            superblock::SUPERBLOCK_OFFSET,
            &mut superblock_bytes,
        )?;
        let superblock = Ext4DiskSuperblock::decode(&superblock_bytes)?;
        superblock.features().validate_mount()?;
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

        let group_geometry = groups.iter().map(GroupGeometry::from_descriptor).collect();
        let group_states: Vec<GroupMutableState> = groups
            .iter()
            .map(GroupMutableState::from_descriptor)
            .collect();
        // Mount-time-only fold seeding the constant-time aggregate, like
        // Linux `ext4_count_dirs`; the live paths never walk the groups.
        let used_directories_count = group_states.iter().fold(0u32, |total, state| {
            total.saturating_add(state.used_directories_count())
        });
        let block_free_extent_caches = vec![None; group_states.len()];
        let mut filesystem = Self {
            allocator: Mutex::new(AllocatorState {
                free_blocks_count: superblock.on_disk_free_blocks_count(),
                free_inodes_count: superblock.on_disk_free_inodes_count(),
                used_directories_count,
                last_orphan: superblock.last_orphan(),
                needs_recovery: superblock.features().needs_recovery(),
                has_orphan_present: superblock.features().has_orphan_present(),
                groups: group_states,
                block_free_extent_caches,
            }),
            delalloc_reserved_blocks: Mutex::new(0),
            superblock,
            group_geometry,
            statfs_mode: Ext4StatFsMode::Bsd,
            bitmap_maxbytes: 0,
            hash_unsigned: 0,
            device: filesystem_device,
            metadata_io,
            journal: None,
            orphan_info: None,
            layout,
            system_zones: Vec::new(),
        };
        filesystem.initialize_directory_hash_policy()?;
        let mut system_zones = filesystem.build_system_zones()?;
        let journal_location = filesystem.journal_location()?;
        if filesystem.needs_recovery() && !allow_recovery {
            return Err(Ext4Error::NeedsRecovery);
        }
        filesystem.journal = match journal_location {
            JournalLocation::None => None,
            JournalLocation::Internal { inode } => Some(MountedJournal::new(
                filesystem.load_internal_journal(inode, &mut system_zones)?,
                filesystem.layout().block_count,
            )?),
            JournalLocation::External { .. } => {
                return Err(Ext4Error::Unsupported(UnsupportedKind::ExternalJournal));
            }
        };
        filesystem.bitmap_maxbytes = filesystem.legacy_max_file_size()?;
        filesystem.system_zones = system_zones;
        if !filesystem.needs_recovery() {
            filesystem.initialize_orphan_info()?;
        }
        if !allow_recovery
            && (filesystem.orphan_head().is_some()
                || !filesystem.orphan_file_empty()?
                || filesystem.has_orphan_present())
        {
            return Err(Ext4Error::NeedsRecovery);
        }
        Ok(filesystem)
    }

    /// Returns the ext4-private legacy indirect-block file-size limit.
    pub(crate) const fn bitmap_max_file_size(&self) -> u64 {
        self.bitmap_maxbytes
    }

    fn initialize_directory_hash_policy(&mut self) -> Ext4Result<()> {
        let has_dir_index = self.superblock.features().has_dir_index();
        let flags = self.superblock.flags();
        self.hash_unsigned = Self::resolve_directory_hash_unsigned(
            self.superblock.default_hash_version(),
            has_dir_index,
            flags,
        )?;
        Ok(())
    }

    /// Persists Linux's implicit unsigned hash choice at writable mount finalization.
    pub(crate) fn persist_directory_hash_policy(&mut self) -> Ext4Result<()> {
        let has_dir_index = self.superblock.features().has_dir_index();
        let flags = self.superblock.flags();
        if has_dir_index && flags & (EXT2_FLAGS_SIGNED_HASH | EXT2_FLAGS_UNSIGNED_HASH) == 0 {
            self.persist_unsigned_hash_flag()?;
        }
        Ok(())
    }

    fn resolve_directory_hash_unsigned(
        default_hash_version: u8,
        has_dir_index: bool,
        flags: u32,
    ) -> Ext4Result<u8> {
        if default_hash_version >= DX_HASH_SIPHASH {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryHash));
        }
        if !has_dir_index {
            return Ok(0);
        }
        if flags & EXT2_FLAGS_UNSIGNED_HASH != 0 {
            return Ok(DX_HASH_UNSIGNED_OFFSET);
        } else if flags & EXT2_FLAGS_SIGNED_HASH == 0 {
            // Current Linux ext4 is built with `-funsigned-char`; select the
            // same default. Writable mount finalization persists this choice.
            return Ok(DX_HASH_UNSIGNED_OFFSET);
        }
        Ok(0)
    }

    fn persist_unsigned_hash_flag(&mut self) -> Ext4Result<()> {
        let (block, offset, len) = self.primary_superblock_location()?;
        let end = offset.checked_add(len).ok_or(Ext4Error::Overflow)?;
        let mut bytes = vec![0; self.device.block_size()];
        self.device.read_blocks(block, 1, &mut bytes)?;
        let superblock_bytes = bytes.get_mut(offset..end).ok_or(Ext4Error::OutOfBounds)?;
        let updated = superblock::set_unsigned_hash_flag(superblock_bytes)?;
        self.device.write_contiguous_blocks(block, 1, &bytes)?;
        self.device.flush()?;
        self.metadata_io.invalidate_all();
        self.superblock = updated;
        Ok(())
    }

    /// Returns the decoded primary superblock.
    ///
    /// The mirror is frozen after mount finalization: geometry and static
    /// feature bits do not change while mounted. Live free-block/free-inode
    /// counters, the orphan-list head, `RECOVER`, and `ORPHAN_PRESENT` move
    /// with the allocator; use [`Self::free_blocks_count`],
    /// [`Self::free_inodes_count`], [`Self::needs_recovery`], and
    /// [`Self::has_orphan_present`] instead of their on-disk snapshots here.
    pub(crate) const fn superblock(&self) -> &Ext4DiskSuperblock {
        &self.superblock
    }

    /// Returns the live free-block aggregate maintained by the allocator.
    pub(crate) fn free_blocks_count(&self) -> u64 {
        lock(&self.allocator).free_blocks_count
    }

    /// Returns the live free-inode aggregate maintained by the allocator.
    pub(crate) fn free_inodes_count(&self) -> u32 {
        lock(&self.allocator).free_inodes_count
    }

    /// Returns whether the filesystem currently carries the recovery flag.
    pub(crate) fn needs_recovery(&self) -> bool {
        lock(&self.allocator).needs_recovery
    }

    /// Returns whether the orphan file may contain entries needing cleanup.
    pub(crate) fn has_orphan_present(&self) -> bool {
        lock(&self.allocator).has_orphan_present
    }

    /// Returns the immutable derived layout.
    pub(crate) const fn layout(&self) -> FilesystemLayout {
        self.layout
    }

    /// Returns the timestamp limits implied by this filesystem's inode size.
    pub(crate) const fn timestamp_limits(&self) -> TimestampLimits {
        timestamp_limits_for_inode_size(self.superblock.inode_size())
    }

    /// Returns the frozen geometry of one block group without taking the
    /// allocator lock.
    ///
    /// The block/inode bitmap and inode-table addresses never change after
    /// mount; Linux reads the same fields from the page-cached group
    /// descriptor table without any lock (`ext4_get_group_desc(..., NULL)`,
    /// e.g. `ext4_get_inode_loc`). Use this on the inode-table hot path
    /// instead of locking the allocator and cloning a whole descriptor just
    /// to expose one static address.
    pub(crate) fn group_geometry(&self, group: BlockGroupNumber) -> Option<&GroupGeometry> {
        self.group_geometry.get(usize::try_from(group.get()).ok()?)
    }

    /// Returns a snapshot of the mutable per-group allocator state.
    ///
    /// Takes the allocator lock and clones every group entry, so it is only
    /// used from tests and is not compiled into the production kernel
    /// build; production code reads live counters in-guard and frozen
    /// addresses through [`Self::group_geometry`].
    #[cfg(unittest)]
    pub(crate) fn groups(&self) -> Vec<GroupMutableState> {
        lock(&self.allocator).groups.clone()
    }

    /// Returns the mount-wide delayed-allocation reservation total.
    pub(crate) fn delalloc_reserved_blocks(&self) -> u64 {
        *lock(&self.delalloc_reserved_blocks)
    }

    /// Returns physical blocks not promised to delayed allocation or the
    /// filesystem reserved-block pool.
    ///
    /// The allocator and delayed-allocation counters are sampled under their
    /// shared lock order so optional preallocation cannot consume a stale
    /// reservation snapshot.
    pub(crate) fn unreserved_blocks_count(&self) -> u64 {
        let allocator = lock(&self.allocator);
        let delalloc_reserved_blocks = lock(&self.delalloc_reserved_blocks);
        allocator
            .free_blocks_count
            .saturating_sub(*delalloc_reserved_blocks)
            .saturating_sub(self.superblock.reserved_blocks_count())
    }

    /// Returns the physical block for one internal journal block.
    pub(crate) fn map_journal_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock> {
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

    /// Returns ext4 filesystem statistics using the default BSD accounting mode.
    ///
    /// # Errors
    ///
    /// Returns an error when validated filesystem geometry cannot be represented
    /// consistently in the reported block counts.
    pub(crate) fn statfs(&self) -> Ext4Result<Ext4StatFs> {
        self.statfs_with_mode(self.statfs_mode)
    }

    /// Returns ext4 filesystem statistics using the requested total-block convention.
    ///
    /// The selected mode changes only the `blocks` total. Free and available
    /// counts retain ext4 reserved-block and delayed-allocation accounting,
    /// read from the same allocator aggregates that admission and allocation
    /// mutation maintain rather than folding every block-group descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error when validated filesystem geometry cannot be represented
    /// consistently in the reported block counts.
    pub(crate) fn statfs_with_mode(&self, mode: Ext4StatFsMode) -> Ext4Result<Ext4StatFs> {
        // Read order is fixed to allocator lock first, then the delalloc
        // mutex; no path takes them in the opposite order.
        let alloc = lock(&self.allocator);
        let free_blocks_count = alloc.free_blocks_count;
        let free_inodes_count = alloc.free_inodes_count;
        drop(alloc);
        let delalloc_reserved_blocks = *lock(&self.delalloc_reserved_blocks);
        let overhead = match mode {
            Ext4StatFsMode::Bsd => self.statfs_overhead_blocks()?,
            Ext4StatFsMode::Minix => 0,
        };
        let blocks = self
            .superblock
            .blocks_count()
            .checked_sub(overhead)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
        let free_blocks = free_blocks_count.saturating_sub(delalloc_reserved_blocks);
        let blocks_available = free_blocks.saturating_sub(self.superblock.reserved_blocks_count());

        Ok(Ext4StatFs {
            block_size: self.superblock().block_size(),
            fragment_size: self.superblock().block_size(),
            blocks,
            blocks_free: free_blocks,
            blocks_available,
            files: u64::from(self.superblock().inodes_count()),
            files_free: u64::from(free_inodes_count),
            max_name_len: crate::disk::dir::DIRENT_NAME_MAX as u32,
        })
    }

    #[cfg(unittest)]
    pub(crate) fn delalloc_reserved_block_count(&self) -> u64 {
        *lock(&self.delalloc_reserved_blocks)
    }

    /// Reads complete filesystem blocks without exposing metadata mutation.
    pub(crate) fn read_blocks(
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

    /// Warms the metadata cache for `blocks` before the caller takes the
    /// allocator lock, so a cold-cache device read runs while other
    /// allocators can still proceed instead of serializing every one of
    /// them behind the mutex (Linux reads the bitmap buffer before
    /// `ext4_lock_group` for the same reason). Cache slots stay resident,
    /// so the authoritative in-lock re-reads hit memory; an error here
    /// fails the caller before it ever takes the lock. The prefetched
    /// bytes are a latency pre-warm only — never an authoritative
    /// snapshot; safety relies on the `MetadataBlockCache` publish-then-
    /// read contract (every publish replaces the slot bytes) that makes
    /// the in-lock re-read observe the latest committed content.
    pub(crate) fn prefetch_metadata_blocks(&self, blocks: &[FilesystemBlock]) -> Ext4Result<()> {
        for block in blocks {
            let _ = self.read_metadata_block(*block)?;
        }
        Ok(())
    }

    pub(crate) fn reload_mutable_metadata_state(&mut self) -> Ext4Result<()> {
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        let superblock_buffer = self.read_metadata_block(superblock_block)?;
        let superblock_bytes = superblock_buffer
            .as_ref()
            .get(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let superblock = Ext4DiskSuperblock::decode(superblock_bytes)?;
        superblock.features().validate_mount()?;
        let layout = FilesystemLayout::derive(&superblock)?;
        if layout != self.layout {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        let groups = self.decode_group_descriptors(&superblock, layout)?;
        self.initialize_directory_hash_policy()?;

        // The frozen geometry table is the mount's single address source.
        // No KExt4-supported operation journals address changes, so a
        // replayed descriptor table with different bitmap or inode-table
        // addresses is either corruption or an unsupported on-disk feature;
        // reject it instead of splitting geometry from the reloaded state.
        //
        // The layout check above pins group_count, so the decoded table and
        // the frozen table are the same length; index them directly rather
        // than zip, which would silently truncate on a mismatch.
        debug_assert_eq!(groups.len(), self.group_geometry.len());
        for (group, descriptor) in groups.iter().enumerate() {
            if GroupGeometry::from_descriptor(descriptor) != self.group_geometry[group] {
                return Err(Ext4Error::Corrupt(
                    CorruptKind::GroupDescriptorAddressChanged,
                ));
            }
        }
        let mut alloc = lock(&self.allocator);
        alloc.free_blocks_count = superblock.on_disk_free_blocks_count();
        alloc.free_inodes_count = superblock.on_disk_free_inodes_count();
        alloc.used_directories_count = groups.iter().fold(0u32, |total, descriptor| {
            total.saturating_add(descriptor.used_directories_count())
        });
        alloc.last_orphan = superblock.last_orphan();
        alloc.needs_recovery = superblock.features().needs_recovery();
        alloc.has_orphan_present = superblock.features().has_orphan_present();
        alloc.groups = groups
            .iter()
            .map(GroupMutableState::from_descriptor)
            .collect();
        crate::mballoc::reset_block_allocation_caches_inner(&mut alloc);
        Ok(())
    }

    fn decode_group_descriptors(
        &self,
        superblock: &Ext4DiskSuperblock,
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
            self.superblock().first_data_block(),
            self.superblock().blocks_count(),
            &self.system_zones,
            inode,
            block,
            count,
        )
    }

    fn build_system_zones(&self) -> Ext4Result<Vec<SystemZone>> {
        let block_count = self.layout.block_count;
        let mut zones: Vec<SystemZone> = Vec::new();
        for group in 0..self.layout.group_count {
            let group_first = self.group_first_block(group)?;
            let base_metadata_blocks = self.base_metadata_blocks(group)?;
            if base_metadata_blocks != 0 {
                add_system_zone_to(
                    &mut zones,
                    group_first,
                    base_metadata_blocks,
                    None,
                    block_count,
                )?;
            }

            let (block_bitmap, inode_bitmap, inode_table) = {
                let geometry = self
                    .group_geometry(BlockGroupNumber::new(group))
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
                (
                    geometry.block_bitmap(),
                    geometry.inode_bitmap(),
                    geometry.inode_table(),
                )
            };
            add_system_zone_to(&mut zones, block_bitmap, 1, None, block_count)?;
            add_system_zone_to(&mut zones, inode_bitmap, 1, None, block_count)?;
            add_system_zone_to(
                &mut zones,
                inode_table,
                u64::from(self.layout.inode_table_blocks_per_group),
                None,
                block_count,
            )?;
        }

        Ok(zones)
    }

    fn journal_location(&self) -> Ext4Result<JournalLocation> {
        let has_journal = self.superblock().features().has_journal();
        let fields = self.superblock().journal();
        select_journal_location(has_journal, fields.inode(), fields.device(), fields.uuid())
    }

    fn load_internal_journal(
        &mut self,
        inode_number: InodeNumber,
        system_zones: &mut Vec<SystemZone>,
    ) -> Ext4Result<InternalJournal> {
        let journal_inode = self.internal_iget(inode_number)?;
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
            add_system_zone_to(
                system_zones,
                extent.physical_start,
                u64::from(extent.len),
                Some(inode_number),
                self.layout.block_count,
            )?;
        }

        let superblock = JournalSuperblock::decode(
            self.read_metadata_block(FilesystemBlock::new(extents[0].physical_start))?
                .as_ref(),
            self.layout.block_size,
            journal_blocks,
            self.superblock().uuid(),
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

    fn base_metadata_blocks(&self, group: u32) -> Ext4Result<u64> {
        if !self.group_has_super(group)? {
            return Ok(0);
        }
        u64::from(1u32)
            .checked_add(u64::from(self.layout.descriptor_table_blocks))
            .and_then(|blocks| {
                blocks.checked_add(u64::from(self.superblock().reserved_gdt_blocks()))
            })
            .ok_or(Ext4Error::Overflow)
    }

    fn group_has_super(&self, group: u32) -> Ext4Result<bool> {
        if group == 0 {
            return Ok(true);
        }
        let features = self.superblock().features();
        if features.has_sparse_super2() {
            return Ok(self.superblock().backup_groups().contains(&group));
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
            .checked_mul(u64::from(self.superblock().blocks_per_group()))
            .and_then(|offset| offset.checked_add(u64::from(self.superblock().first_data_block())))
            .ok_or(Ext4Error::Overflow)
    }

    pub(crate) fn block_group_range(&self, group: BlockGroupNumber) -> Ext4Result<BlockGroupRange> {
        if group.get() >= self.layout.group_count {
            return Err(Ext4Error::OutOfBounds);
        }
        let first = self.group_first_block(group.get())?;
        let end = first
            .checked_add(u64::from(self.superblock().blocks_per_group()))
            .ok_or(Ext4Error::Overflow)?
            .min(self.superblock().blocks_count());
        let block_count = u32::try_from(end.checked_sub(first).ok_or(Ext4Error::Overflow)?)
            .map_err(|_| Ext4Error::Overflow)?;
        BlockGroupRange::new(group, FilesystemBlock::new(first), block_count)
    }

    pub(crate) fn block_bitmap_checksum_bytes(&self) -> Ext4Result<usize> {
        bitmap_checksum_bytes(
            self.superblock().clusters_per_group(),
            CorruptKind::InvalidBlockGroupGeometry,
        )
    }

    pub(crate) fn inode_bitmap_checksum_bytes(&self) -> Ext4Result<usize> {
        bitmap_checksum_bytes(
            self.superblock().inodes_per_group(),
            CorruptKind::InvalidInodeGeometry,
        )
    }

    pub(crate) fn block_group_for_block(
        &self,
        block: FilesystemBlock,
    ) -> Ext4Result<BlockGroupNumber> {
        let first_data = u64::from(self.superblock().first_data_block());
        if block.get() < first_data || block.get() >= self.superblock().blocks_count() {
            return Err(Ext4Error::OutOfBounds);
        }
        let group = (block.get() - first_data) / u64::from(self.superblock().blocks_per_group());
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
                if goal.get() >= u64::from(self.superblock().first_data_block())
                    && goal.get() < self.superblock().blocks_count() =>
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
            .checked_mul(self.superblock().inodes_per_group())
            .and_then(|offset| offset.checked_add(1))
            .ok_or(Ext4Error::Overflow)?;
        let remaining = self
            .superblock()
            .inodes_count()
            .checked_sub(first_inode - 1)
            .ok_or(Ext4Error::OutOfBounds)?;
        let inode_count = self.superblock().inodes_per_group().min(remaining);
        InodeGroupRange::new(group, InodeNumber::new(first_inode), inode_count)
    }

    pub(crate) fn block_group_for_inode(&self, inode: InodeNumber) -> Ext4Result<BlockGroupNumber> {
        if inode.get() == 0 || inode.get() > self.superblock().inodes_count() {
            return Err(Ext4Error::OutOfBounds);
        }
        let group = (inode.get() - 1) / self.superblock().inodes_per_group();
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

        if self.superblock().features().has_flex_bg() {
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
        // Constant-time aggregate reads, like Linux `find_group_orlov`
        // reading the `s_freeinodes_counter`/`s_freeclusters_counter`/
        // `s_dirs_counter` percpu counters instead of folding groups.
        let alloc = lock(&self.allocator);
        InodeAllocationTotals {
            free_inodes: u64::from(alloc.free_inodes_count),
            free_blocks: alloc.free_blocks_count,
            used_directories: u64::from(alloc.used_directories_count),
        }
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
            return self.superblock().checksum_seed();
        };
        self.orlov_hash(name).major()
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
            .saturating_add(u64::from(self.superblock().inodes_per_group()) * flex_size / 16);
        let min_inodes = avg_free_inodes
            .saturating_sub(u64::from(self.superblock().inodes_per_group()) * flex_size / 4);
        let min_blocks = avg_free_blocks
            .saturating_sub(u64::from(self.superblock().blocks_per_group()) * flex_size / 4);

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
            self.group_mutable_state(*group)
                .map(|state| u64::from(state.free_inodes_count()) >= min_free_inodes)
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
            let state = self.group_mutable_state(group)?;
            if state.free_inodes_count() == 0 || state.free_blocks_count() == 0 {
                continue;
            }
            best = match best {
                Some((_, best_used_dirs)) if best_used_dirs <= state.used_directories_count() => {
                    best
                }
                _ => Some((group, state.used_directories_count())),
            };
        }
        Ok(best.map(|(group, _)| group))
    }

    fn group_has_free_inode_and_block(&self, group: BlockGroupNumber) -> Ext4Result<bool> {
        let state = self.group_mutable_state(group)?;
        Ok(state.free_inodes_count() > 0 && state.free_blocks_count() > 0)
    }

    fn group_mutable_state(&self, group: BlockGroupNumber) -> Ext4Result<GroupMutableState> {
        lock(&self.allocator)
            .groups
            .get(usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?)
            .copied()
            .ok_or(Ext4Error::OutOfBounds)
    }

    fn flex_group_index(&self, group: BlockGroupNumber) -> u32 {
        group.get() / self.flex_group_size()
    }

    fn flex_group_size(&self) -> u32 {
        if !self.superblock().features().has_flex_bg() {
            return 1;
        }
        let size = 1u32.checked_shl(u32::from(self.superblock().log_groups_per_flex()));
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
            let state = self.group_mutable_state(group)?;
            stats.free_inodes = stats
                .free_inodes
                .saturating_add(u64::from(state.free_inodes_count()));
            stats.free_blocks = stats
                .free_blocks
                .saturating_add(u64::from(state.free_blocks_count()));
            stats.used_directories = stats
                .used_directories
                .saturating_add(u64::from(state.used_directories_count()));
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
        let zones = &self.system_zones;
        let index = zones.partition_point(|zone| zone.end <= block);
        zones
            .get(index)
            .is_some_and(|zone| zone.start <= block && block < zone.end)
    }

    pub(crate) fn is_reserved_inode(&self, inode: InodeNumber) -> bool {
        inode.get() != 0 && inode.get() < self.superblock().first_inode()
    }
}

fn add_system_zone_to(
    zones: &mut Vec<SystemZone>,
    start: u64,
    count: u64,
    owner: Option<InodeNumber>,
    block_count: u64,
) -> Ext4Result<()> {
    let zone = SystemZone::new(start, count, owner, block_count)?;
    let index = zones.partition_point(|entry| entry.start < zone.start);

    if index > 0 {
        let previous = zones[index - 1];
        if previous.end > zone.start {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        if previous.end == zone.start && previous.owner == zone.owner {
            zones[index - 1].end = zone.end;
            if index < zones.len() {
                let next = zones[index];
                if zone.end > next.start {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
                }
                if zone.end == next.start && next.owner == zone.owner {
                    let next = zones.remove(index);
                    zones[index - 1].end = next.end;
                }
            }
            return Ok(());
        }
    }

    if index < zones.len() {
        let next = zones[index];
        if zone.end > next.start {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        if zone.end == next.start && zone.owner == next.owner {
            zones[index].start = zone.start;
            return Ok(());
        }
    }

    zones.insert(index, zone);
    Ok(())
}

impl Ext4SbInfo {
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
        u64::from(self.superblock().first_data_block())
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
            BlockMapping::Mapped { physical, len, .. } if len.get() != 0 => {
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

#[unittest::mod_test]
mod unittests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn mount_hash_policy_matches_linux_signedness_order() {
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, false, 0),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, false, EXT2_FLAGS_SIGNED_HASH),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, false, EXT2_FLAGS_UNSIGNED_HASH),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(
                0,
                false,
                EXT2_FLAGS_SIGNED_HASH | EXT2_FLAGS_UNSIGNED_HASH,
            ),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, true, 0),
            Ok(DX_HASH_UNSIGNED_OFFSET)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, true, EXT2_FLAGS_SIGNED_HASH),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(0, true, EXT2_FLAGS_UNSIGNED_HASH),
            Ok(DX_HASH_UNSIGNED_OFFSET)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(
                0,
                true,
                EXT2_FLAGS_SIGNED_HASH | EXT2_FLAGS_UNSIGNED_HASH,
            ),
            Ok(DX_HASH_UNSIGNED_OFFSET)
        );
    }

    #[def_test]
    fn mount_hash_policy_rejects_invalid_default_hash() {
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(5, false, 0),
            Ok(0)
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(DX_HASH_SIPHASH, false, 0),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryHash))
        );
        assert_eq!(
            Ext4SbInfo::resolve_directory_hash_unsigned(u8::MAX, true, 0),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryHash))
        );
    }
}

#[cfg(unittest)]
pub(crate) mod test_fixtures;
#[cfg(unittest)]
mod tests;

fn validate_group(
    superblock: &Ext4DiskSuperblock,
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

#[cfg(unittest)]
pub(crate) mod fault_device;
