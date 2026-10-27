// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block allocator entry points for the mounted ext4 filesystem.

use alloc::{vec, vec::Vec};

use crate::{
    bitmap_allocator::{
        self, BlockAllocation, BlockGroupRange, BlockRunAllocation, range_end_containing,
    },
    disk::{
        GroupMutableState, checksum, decrement_group_free_blocks_count,
        increment_group_free_blocks_count, set_group_free_blocks_count, superblock,
        update_group_block_bitmap_metadata,
    },
    error::{CorruptKind, Ext4Error, Ext4Result},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest, ensure_block_group_free_cache_inner},
    prealloc_space::bit_in_ranges,
    superblock::{
        AllocatorState, Ext4SbInfo, bitmap_bit_capacity, count_clear_ext4_bitmap_bits,
        ensure_metadata_credits, ext4_bitmap_checksum_matches, ext4_mark_bitmap_end, lock,
        replace_metadata_access_bytes, set_ext4_bitmap_bit, validate_ext4_bitmap_range_set,
    },
    types::{BlockCount, BlockGroupNumber, FilesystemBlock, PhysicalBlock},
};

pub(crate) mod pending_free;

const BLOCK_ALLOCATOR_METADATA_CREDITS: u32 = 3;
/// Allocator metadata plus create access to one new metadata block.
pub(crate) const BLOCK_ALLOCATION_METADATA_CREDITS: u32 = BLOCK_ALLOCATOR_METADATA_CREDITS + 1;
/// How freed blocks participate in metadata checkpoint protection.
#[derive(Clone, Copy)]
pub(crate) enum BlockReleaseKind {
    Data,
    Metadata,
    MetadataWithoutRevoke,
}

impl Ext4SbInfo {
    #[allow(dead_code)]
    pub(crate) fn allocate_block(
        &self,
        goal: Option<FilesystemBlock>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        let request = Ext4AllocationRequest::for_metadata(
            goal,
            BlockCount::new(1),
            BlockCount::new(1),
            Ext4AllocationFlags::EXACT,
        )?;
        self.allocate_blocks_for_write(request, handle)
            .map(|allocation| allocation.first_block_allocation())
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_block_in_group(
        &self,
        group: BlockGroupNumber,
        goal: Option<FilesystemBlock>,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        let allocation = self.allocate_block_run_in_group(
            group,
            goal,
            BlockCount::new(1),
            BlockCount::new(1),
            handle,
        )?;
        Ok(allocation.first_block_allocation())
    }

    /// Warms the metadata cache for one group allocation attempt (block
    /// bitmap, group descriptor, superblock).
    ///
    /// - `group`: the group about to be attempted.
    ///
    /// Call this *outside* the allocator lock: a cold-cache device read must
    /// not serialize every allocator behind the mutex. The allocation
    /// re-reads all three authoritatively under the lock, so this is pure
    /// warm-up.
    ///
    /// Locking: must NOT hold the allocator mutex.
    pub(crate) fn prefetch_group_allocation_metadata(
        &self,
        group: BlockGroupNumber,
    ) -> Ext4Result<()> {
        let bitmap_block = FilesystemBlock::new(
            self.group_geometry(group)
                .ok_or(Ext4Error::OutOfBounds)?
                .block_bitmap(),
        );
        let (descriptor_block, ..) = self.group_descriptor_location(group)?;
        let (superblock_block, ..) = self.primary_superblock_location()?;
        self.prefetch_metadata_blocks(&[bitmap_block, descriptor_block, superblock_block])
    }

    /// Allocates a contiguous run inside `group`, journalling the bitmap, group
    /// descriptor and superblock updates through `handle`.
    ///
    /// - `group`: group to allocate in; no other group is searched.
    /// - `goal`: preferred first block, filesystem-wide; ignored when it lies
    ///   outside `group`.
    /// - `min_len`: smallest acceptable run, non-zero and at most `expected_len`.
    /// - `expected_len`: requested run length, non-zero.
    /// - `handle`: journal handle carrying the updates; needs
    ///   [`BLOCK_ALLOCATOR_METADATA_CREDITS`] credits.
    ///
    /// Returns: the allocated run, between `min_len` and `expected_len` blocks.
    /// Its blocks are marked allocated, so the caller owns them and must release
    /// them if a later step fails.
    ///
    /// Errors: `NoSpace` when no run of `min_len` exists in the group; other
    /// errors propagate unchanged.
    ///
    /// Locking: caller must not hold the allocator mutex. Prefetch runs unlocked.
    /// May sleep.
    pub(crate) fn allocate_block_run_in_group(
        &self,
        group: BlockGroupNumber,
        goal: Option<FilesystemBlock>,
        min_len: BlockCount,
        expected_len: BlockCount,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockRunAllocation> {
        if min_len.get() == 0 || expected_len.get() == 0 || min_len > expected_len {
            return Err(Ext4Error::OutOfBounds);
        }
        self.prefetch_group_allocation_metadata(group)?;
        let mut alloc = lock(&self.allocator);
        self.allocate_block_run_in_group_locked(
            &mut alloc,
            group,
            goal,
            min_len,
            expected_len,
            handle,
        )
    }

    /// Group allocation with the allocator lock already held.
    ///
    /// - `alloc`: live allocator guard.
    /// - `group`: the group to allocate from.
    /// - `goal`: preferred first block, if any.
    /// - `min_len`/`expected_len`: run length bounds.
    /// - `handle`: journal handle marking the run.
    ///
    /// Split out of [`Self::allocate_block_run_in_group`] so the
    /// speculative consume path can take a space and mark the run inside a
    /// *single* lock section (exclusion must not lapse between selection and
    /// mark). Callers that know the group beforehand warm the cache with
    /// [`Self::prefetch_group_allocation_metadata`] outside the lock; consume
    /// learns the group from its inode-side space with a leaf-lock peek and
    /// warms it the same way. If that advisory peek misses, the in-lock read
    /// still decides, at the cost of a cold read under the mutex.
    ///
    /// Locking: caller must hold the allocator mutex.
    pub(crate) fn allocate_block_run_in_group_locked(
        &self,
        alloc: &mut AllocatorState,
        group: BlockGroupNumber,
        goal: Option<FilesystemBlock>,
        min_len: BlockCount,
        expected_len: BlockCount,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockRunAllocation> {
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        // Geometry and block locations come from frozen mount tables and need
        // no lock of their own. Callers warm the metadata cache outside the
        // allocator lock (see prefetch_group_allocation_metadata); the
        // authoritative reads happen under this lock regardless, so a cold
        // cache costs latency here, never correctness.
        let range = self.block_group_range(group)?;
        let block_bitmap_checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let bitmap_block = FilesystemBlock::new(
            self.group_geometry(group)
                .ok_or(Ext4Error::OutOfBounds)?
                .block_bitmap(),
        );
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        let descriptor = alloc
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.free_blocks_count() == 0 && !descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::NoSpace);
        }
        if group.get() == 0 && descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        if alloc.free_blocks_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        ensure_metadata_credits(handle, BLOCK_ALLOCATOR_METADATA_CREDITS)?;

        let had_uninit_block_bitmap = descriptor.has_uninit_block_bitmap();
        let mut bitmap_bytes = if had_uninit_block_bitmap {
            vec![0; usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?]
        } else {
            let bitmap = self.read_metadata_block(bitmap_block)?;
            self.verify_block_bitmap_checksum_for_group(
                descriptor.block_bitmap_checksum(),
                bitmap.as_ref(),
            )?;
            self.validate_block_bitmap_for_group(range, bitmap.as_ref())?;
            bitmap.as_ref().to_vec()
        };
        if had_uninit_block_bitmap {
            self.prepare_block_bitmap_for_group(range, &mut bitmap_bytes, true)?;
        }
        let prepared_free_blocks = if had_uninit_block_bitmap {
            Some(count_clear_ext4_bitmap_bits(
                &bitmap_bytes,
                range.block_count(),
            )?)
        } else {
            None
        };
        // The free-extent cache mirrors the bitmap, so it cannot see either
        // prealloc spaces or uncommitted frees. Snapshot both under the
        // allocator lock instead of making protection depend on cache lifetime.
        let excluded_ranges = Self::allocation_exclusion_ranges(alloc, range)?;
        let suggested_goal = ensure_block_group_free_cache_inner(
            alloc,
            group,
            range,
            &bitmap_bytes,
            Some(crate::mballoc::FreeCacheGoalRequest {
                goal,
                min_len,
                expected_len,
            }),
            |block| self.is_system_zone_block(block),
        )?
        // A cache-suggested goal may fall inside an excluded range that the
        // cache cannot see; drop the hint rather than pointing the scanner at
        // a block it must immediately skip.
        .filter(|&g| {
            range
                .bit_index(g)
                .is_ok_and(|bit| !bit_in_ranges(&excluded_ranges, bit))
        });
        // The caller's own goal often lands on a preallocated range start (a
        // writeback goal is the previous run's physical end, which is exactly
        // where this inode's sampler files a space). Advance it past the
        // range, or drop it when the range ends at the group's edge. The
        // scanner also skips exclusions reached after this initial hint.
        // A goal outside this group retains the default scan from zero.
        let scan_goal = suggested_goal.or(goal).and_then(|goal| {
            let bit = range.bit_index(goal).ok()?;
            match range_end_containing(&excluded_ranges, bit) {
                Some(end_bit) => range.block_at(end_bit).ok(),
                None => Some(goal),
            }
        });
        let allocation = match bitmap_allocator::allocate_block_run_from_bitmap(
            &mut bitmap_bytes,
            range,
            scan_goal,
            min_len,
            expected_len,
            &excluded_ranges,
            |block| self.is_system_zone_block(block),
        ) {
            Ok(allocation) => allocation,
            Err(Ext4Error::NoSpace) if min_len.get() == 1 && excluded_ranges.is_empty() => {
                // Only system zones can reject a clear-bit candidate (their
                // bits are always set in a valid bitmap, so is_protected is
                // unreachable for them). Withheld frees and prealloc spaces
                // are clear but unavailable; exhausting those is ordinary
                // ENOSPC rather than bitmap corruption.
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
            }
            Err(Ext4Error::NoSpace) => return Err(Ext4Error::NoSpace),
            Err(error) => return Err(error),
        };
        // Linux rejects an empty allocation with BUG_ON and treats any
        // system-zone overlap as a filesystem error in
        // ext4_mb_mark_diskspace_used; here both are internal invariants
        // of the bitmap scan.
        debug_assert!(
            allocation.block_count() >= min_len && allocation.block_count() <= expected_len,
            "allocated run {} outside requested bounds [{}, {}]",
            allocation.block_count(),
            min_len,
            expected_len
        );
        debug_assert!(
            (0..allocation.block_count().get()).all(|offset| range
                .block_at(allocation.first_bitmap_bit() + offset)
                .is_ok_and(|block| {
                    !self.is_system_zone_block(block)
                        && range
                            .bit_index(block)
                            .is_ok_and(|bit| !bit_in_ranges(&excluded_ranges, bit))
                })),
            "block allocator handed out system-zone, preallocated or uncommitted freed blocks"
        );

        let mut descriptor_bytes = self
            .read_metadata_block(descriptor_block)?
            .as_ref()
            .to_vec();
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let Some(free_blocks) = prepared_free_blocks {
            let _ = set_group_free_blocks_count(
                descriptor_slice,
                group.get(),
                self.superblock().checksum_seed(),
                self.superblock().features().has_64bit(),
                self.superblock().features().has_metadata_checksum(),
                free_blocks,
            )?;
        }
        let _ = decrement_group_free_blocks_count(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            allocation.block_count().get(),
        )?;
        let updated_descriptor = update_group_block_bitmap_metadata(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            block_bitmap_checksum_bytes,
            &bitmap_bytes,
        )?;

        let mut superblock_bytes = self
            .read_metadata_block(superblock_block)?
            .as_ref()
            .to_vec();
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        if let Some(free_blocks) = prepared_free_blocks {
            let previous = descriptor.free_blocks_count();
            if free_blocks > previous {
                let delta = free_blocks
                    .checked_sub(previous)
                    .ok_or(Ext4Error::Overflow)?;
                let _ = superblock::increment_free_blocks_count(superblock_slice, delta)?;
            } else if previous > free_blocks {
                let delta = previous
                    .checked_sub(free_blocks)
                    .ok_or(Ext4Error::Overflow)?;
                let _ = superblock::decrement_free_blocks_count(superblock_slice, delta)?;
            }
        }
        let updated_superblock = superblock::decrement_free_blocks_count(
            superblock_slice,
            allocation.block_count().get(),
        )?;
        let mut updated_free_cache = alloc
            .block_free_extent_caches
            .get(group_index)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        updated_free_cache
            .mark_allocated(allocation.first_bitmap_bit(), allocation.block_count())?;

        // Publication stays inside the allocator lock: the bitmap bytes are
        // the bit-level source of truth, so releasing the lock before the
        // replacements land would let a concurrent allocator observe the
        // updated counters against a stale bitmap and double-allocate.
        let bitmap_access = self.metadata_io.write_access(bitmap_block, handle)?;
        let descriptor_access = self.metadata_io.write_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.write_access(superblock_block, handle)?;
        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        alloc.block_free_extent_caches[group_index] = Some(updated_free_cache);
        alloc.groups[group_index] = GroupMutableState::from_descriptor(&updated_descriptor);
        alloc.free_blocks_count = updated_superblock.on_disk_free_blocks_count();
        Ok(allocation)
    }

    pub(crate) fn release_allocated_block(
        &self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, 1, handle, BlockReleaseKind::Data)
    }

    pub(crate) fn release_allocated_metadata_block(
        &self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(block, 1, handle, BlockReleaseKind::Metadata)
    }

    pub(crate) fn release_allocated_metadata_block_without_revoke(
        &self,
        block: PhysicalBlock,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<BlockAllocation> {
        self.release_allocated_block_inner(
            block,
            1,
            handle,
            BlockReleaseKind::MetadataWithoutRevoke,
        )
    }

    /// Releases a contiguous extent in bounded group-local bitmap updates.
    /// Failure after a completed group has joined metadata and must abort through
    /// the caller's mutation completion, like every other partial metadata edit.
    pub(crate) fn release_allocated_block_range(
        &self,
        first: PhysicalBlock,
        count: BlockCount,
        kind: BlockReleaseKind,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let end = first
            .get()
            .checked_add(u64::from(count.get()))
            .ok_or(Ext4Error::Overflow)?;
        let mut next = first.get();
        while next < end {
            let block = FilesystemBlock::new(next);
            let range = self.block_group_range(self.block_group_for_block(block)?)?;
            let count = (end - next)
                .min(u64::from(range.block_count() - range.bit_index(block)?))
                .min(256) as u32;
            self.release_allocated_block_inner(PhysicalBlock::new(next), count, handle, kind)?;
            next += u64::from(count);
        }
        Ok(())
    }

    fn release_allocated_block_inner(
        &self,
        block: PhysicalBlock,
        count: u32,
        handle: &mut crate::jbd2::JournalHandle<'_>,
        kind: BlockReleaseKind,
    ) -> Ext4Result<BlockAllocation> {
        let block = FilesystemBlock::new(block.get());
        // Linux ext4_sb_block_valid() lets the owning inode map its own
        // system-zone blocks, but the owner exception applies only to
        // block-reference validation and never to the release path: a freed
        // system-zone block would remain in the read-only zone list and be
        // handed out again by the allocator as free, corrupting the bitmap.
        // Releases of any system-zone block are therefore rejected
        // unconditionally, matching Linux.
        let end = block
            .get()
            .checked_add(u64::from(count))
            .ok_or(Ext4Error::Overflow)?;
        if (block.get()..end).any(|block| self.is_system_zone_block(FilesystemBlock::new(block))) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        let required_credits = BLOCK_ALLOCATOR_METADATA_CREDITS
            + if matches!(kind, BlockReleaseKind::Metadata) {
                count
            } else {
                0
            };
        ensure_metadata_credits(handle, required_credits)?;
        let group = self.block_group_for_block(block)?;
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        // Geometry and block locations come from frozen mount tables and
        // need no lock; the freed block is known up front, so every block
        // this path touches is addressable before the allocator lock.
        let range = self.block_group_range(group)?;
        let block_bitmap_checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let bitmap_block = FilesystemBlock::new(
            self.group_geometry(group)
                .ok_or(Ext4Error::OutOfBounds)?
                .block_bitmap(),
        );
        let (descriptor_block, descriptor_offset, descriptor_len) =
            self.group_descriptor_location(group)?;
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        // Warm the metadata cache outside the allocator lock so a
        // cold-cache device read does not serialize every allocator behind
        // the mutex; the authoritative snapshots are re-read inside the
        // lock below.
        self.prefetch_metadata_blocks(&[bitmap_block, descriptor_block, superblock_block])?;

        // The allocator lock still spans the in-lock read-modify-write so
        // concurrent releases keep the bitmap and counters consistent with
        // one another.
        let mut alloc = lock(&self.allocator);
        let descriptor = alloc
            .groups
            .get(group_index)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        if descriptor.has_uninit_block_bitmap() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }

        let bitmap = self.read_metadata_block(bitmap_block)?;
        self.verify_block_bitmap_checksum_for_group(
            descriptor.block_bitmap_checksum(),
            bitmap.as_ref(),
        )?;
        self.validate_block_bitmap_for_group(range, bitmap.as_ref())?;

        let mut bitmap_bytes = bitmap.as_ref().to_vec();
        ensure_block_group_free_cache_inner(
            &mut alloc,
            group,
            range,
            &bitmap_bytes,
            None,
            |candidate| self.is_system_zone_block(candidate),
        )?;
        let released = bitmap_allocator::release_block_to_bitmap(
            &mut bitmap_bytes,
            range,
            block,
            |candidate| self.is_system_zone_block(candidate) && candidate != block,
        )?;
        for next in block.get() + 1..end {
            bitmap_allocator::release_block_to_bitmap(
                &mut bitmap_bytes,
                range,
                FilesystemBlock::new(next),
                |candidate| self.is_system_zone_block(candidate),
            )?;
        }

        let mut descriptor_bytes = self
            .read_metadata_block(descriptor_block)?
            .as_ref()
            .to_vec();
        let descriptor_slice = descriptor_bytes
            .get_mut(descriptor_offset..descriptor_offset + descriptor_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let _ = increment_group_free_blocks_count(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            count,
        )?;
        let updated_descriptor = update_group_block_bitmap_metadata(
            descriptor_slice,
            group.get(),
            self.superblock().checksum_seed(),
            self.superblock().features().has_64bit(),
            self.superblock().features().has_metadata_checksum(),
            block_bitmap_checksum_bytes,
            &bitmap_bytes,
        )?;
        if updated_descriptor.free_blocks_count() > range.block_count() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }

        let mut superblock_bytes = self
            .read_metadata_block(superblock_block)?
            .as_ref()
            .to_vec();
        let superblock_slice = superblock_bytes
            .get_mut(superblock_offset..superblock_offset + superblock_len)
            .ok_or(Ext4Error::OutOfBounds)?;
        let updated_superblock = superblock::increment_free_blocks_count(superblock_slice, count)?;
        let mut updated_free_cache = alloc
            .block_free_extent_caches
            .get(group_index)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(Ext4Error::OutOfBounds)?;
        updated_free_cache.mark_free_run(released.bitmap_bit(), count)?;

        // Publication stays inside the allocator lock: the bitmap bytes are
        // the bit-level source of truth, so releasing the lock before the
        // replacements land would let a concurrent allocator observe the
        // updated counters against a stale bitmap and double-free.
        let bitmap_access = self.metadata_io.write_access(bitmap_block, handle)?;
        let descriptor_access = self.metadata_io.write_access(descriptor_block, handle)?;
        let superblock_access = self.metadata_io.write_access(superblock_block, handle)?;
        for next in block.get()..end {
            let block = FilesystemBlock::new(next);
            match kind {
                BlockReleaseKind::Metadata => {
                    self.metadata_io.forget_metadata_block(block, handle)?
                }
                BlockReleaseKind::MetadataWithoutRevoke => self
                    .metadata_io
                    .forget_metadata_block_without_revoke(block, handle)?,
                BlockReleaseKind::Data => {}
            }
        }
        // Until the freeing bitmap is durable, old on-disk extent mappings
        // still own this block. A new data write must not overwrite them even
        // when the current bitmap and free-extent cache already show it free.
        alloc
            .pending_frees
            .insert(group, handle.id(), block, FilesystemBlock::new(end - 1))?;
        replace_metadata_access_bytes(&bitmap_access, bitmap_bytes)?;
        replace_metadata_access_bytes(&descriptor_access, descriptor_bytes)?;
        replace_metadata_access_bytes(&superblock_access, superblock_bytes)?;
        alloc.block_free_extent_caches[group_index] = Some(updated_free_cache);
        alloc.groups[group_index] = GroupMutableState::from_descriptor(&updated_descriptor);
        alloc.free_blocks_count = updated_superblock.on_disk_free_blocks_count();
        Ok(released)
    }

    fn allocation_exclusion_ranges(
        alloc: &AllocatorState,
        range: BlockGroupRange,
    ) -> Ext4Result<Vec<(u32, u32)>> {
        let mut ranges = alloc.prealloc_bit_ranges_in_group(range);
        let mut pending = Vec::new();
        alloc.pending_frees.append_ranges(
            range.group(),
            range.block_at(0)?,
            range.block_at(range.block_count() - 1)?,
            &mut pending,
        );
        for (first, last) in pending {
            ranges.push((range.bit_index(first)?, range.bit_index(last)? + 1));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some(previous) = merged.last_mut()
                && start <= previous.1
            {
                previous.1 = previous.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        Ok(merged)
    }

    pub(crate) fn prepare_block_bitmap_for_group(
        &self,
        range: BlockGroupRange,
        bitmap: &mut [u8],
        initialize: bool,
    ) -> Ext4Result<()> {
        if initialize {
            bitmap.fill(0);
            self.mark_system_zone_bits_in_block_bitmap(range, bitmap)?;
            ext4_mark_bitmap_end(range.block_count(), bitmap_bit_capacity(bitmap)?, bitmap)?;
            return Ok(());
        }

        self.validate_block_bitmap_for_group(range, bitmap)
    }

    fn validate_block_bitmap_for_group(
        &self,
        range: BlockGroupRange,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        self.validate_system_zone_bits_in_block_bitmap(range, bitmap)?;
        validate_ext4_bitmap_range_set(
            bitmap,
            range.block_count(),
            bitmap_bit_capacity(bitmap)?,
            CorruptKind::InvalidBlockBitmap,
        )
    }

    fn verify_block_bitmap_checksum_for_group(
        &self,
        expected_checksum: u32,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }

        let checksum_bytes = self.block_bitmap_checksum_bytes()?;
        let input = bitmap
            .get(..checksum_bytes)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let calculated = checksum::bitmap_checksum(input, self.superblock().checksum_seed());
        if !ext4_bitmap_checksum_matches(
            calculated,
            expected_checksum,
            self.superblock().features().has_64bit(),
        ) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }

        Ok(())
    }

    fn mark_system_zone_bits_in_block_bitmap(
        &self,
        range: BlockGroupRange,
        bitmap: &mut [u8],
    ) -> Ext4Result<()> {
        for bit in 0..range.block_count() {
            let block = range.block_at(bit)?;
            if self.is_system_zone_block(block) {
                set_ext4_bitmap_bit(bitmap, bit)?;
            }
        }
        Ok(())
    }

    fn validate_system_zone_bits_in_block_bitmap(
        &self,
        range: BlockGroupRange,
        bitmap: &[u8],
    ) -> Ext4Result<()> {
        for bit in 0..range.block_count() {
            let block = range.block_at(bit)?;
            if self.is_system_zone_block(block)
                && !crate::superblock::is_ext4_bitmap_bit_set(bitmap, bit)?
            {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
            }
        }
        Ok(())
    }
}

unittest::path_mod_test!(tests, "../ut-tests/balloc/tests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(component_tests, "../ut-tests/balloc/component_tests.rs");
