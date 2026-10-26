// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style multiblock allocation request layer.
//!
//! This module owns ext4 allocation policy: logical range, physical goal,
//! locality hint, minimum acceptable length, and partial-allocation reporting.
//! `balloc` remains the lower bitmap/accounting layer that journals concrete
//! bitmap, group descriptor, and superblock updates.

use alloc::{vec, vec::Vec};

use crate::{
    bitmap_allocator::{BlockAllocation, BlockRunAllocation},
    error::{CorruptKind, Ext4Error, Ext4Result},
    jbd2::JournalHandle,
    prealloc_space::PREALLOC_DISCARD_SEARCH_LIMIT,
    superblock::{AllocatorState, Ext4SbInfo, is_ext4_bitmap_bit_set, lock},
    types::{BlockCount, BlockGroupNumber, FilesystemBlock, LogicalBlock, PhysicalBlock},
};

const MBALLOC_ORDER_BUCKETS: usize = u32::BITS as usize;

/// Stage-R4 allocation policy flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ext4AllocationFlags {
    allow_partial: bool,
}

impl Ext4AllocationFlags {
    pub(crate) const ALLOW_PARTIAL: Self = Self {
        allow_partial: true,
    };
    pub(crate) const EXACT: Self = Self {
        allow_partial: false,
    };

    const fn allows_partial(self) -> bool {
        self.allow_partial
    }
}

/// A Linux `ext4_allocation_request`-style allocation input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ext4AllocationRequest {
    // Logical block number: block index within the file (0-based).
    logical_start: LogicalBlock,
    // Recommended block number.
    goal: Option<FilesystemBlock>,
    expected_len: BlockCount,
    min_len: BlockCount,
    flags: Ext4AllocationFlags,
    // Keep the contents of the same inode as contiguous as possible.
    locality_group: BlockGroupNumber,
}

impl Ext4AllocationRequest {
    /// Builds a file-data allocation request.
    ///
    /// Callers must pass the inode's locality group so the write path never
    /// silently starts scanning from block group 0 when no physical goal exists.
    pub(crate) fn new(
        logical_start: LogicalBlock,
        goal: Option<FilesystemBlock>,
        expected_len: BlockCount,
        min_len: BlockCount,
        flags: Ext4AllocationFlags,
        locality_group: BlockGroupNumber,
    ) -> Ext4Result<Self> {
        Self::validate_lengths(expected_len, min_len, flags)?;
        Ok(Self {
            logical_start,
            goal,
            expected_len,
            min_len,
            flags,
            locality_group,
        })
    }

    pub(crate) fn for_metadata(
        goal: Option<FilesystemBlock>,
        expected_len: BlockCount,
        min_len: BlockCount,
        flags: Ext4AllocationFlags,
    ) -> Ext4Result<Self> {
        Self::validate_lengths(expected_len, min_len, flags)?;
        Ok(Self {
            logical_start: LogicalBlock::new(0),
            goal,
            expected_len,
            min_len,
            flags,
            locality_group: BlockGroupNumber::new(0),
        })
    }

    fn validate_lengths(
        expected_len: BlockCount,
        min_len: BlockCount,
        flags: Ext4AllocationFlags,
    ) -> Ext4Result<()> {
        if min_len.get() == 0 || expected_len.get() == 0 || min_len > expected_len {
            return Err(Ext4Error::OutOfBounds);
        }
        if min_len < expected_len && !flags.allows_partial() {
            return Err(Ext4Error::OutOfBounds);
        }
        Ok(())
    }

    const fn locality_group(self) -> BlockGroupNumber {
        self.locality_group
    }

    /// First file-logical block of the request.
    pub(crate) const fn logical_start(self) -> LogicalBlock {
        self.logical_start
    }
}

/// A concrete physical run returned by the R4 allocator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ext4AllocatedRun {
    logical_start: LogicalBlock,
    requested_len: BlockCount,
    allocation: BlockRunAllocation,
}

impl Ext4AllocatedRun {
    const fn new(
        logical_start: LogicalBlock,
        requested_len: BlockCount,
        allocation: BlockRunAllocation,
    ) -> Self {
        Self {
            logical_start,
            requested_len,
            allocation,
        }
    }

    #[cfg(unittest)]
    pub(crate) const fn logical_start(self) -> LogicalBlock {
        self.logical_start
    }

    #[cfg(unittest)]
    pub(crate) fn group(self) -> BlockGroupNumber {
        self.allocation.group()
    }

    pub(crate) fn physical_start(self) -> PhysicalBlock {
        self.allocation.first_block()
    }

    pub(crate) fn block_count(self) -> BlockCount {
        self.allocation.block_count()
    }

    #[cfg(unittest)]
    pub(crate) const fn requested_len(self) -> BlockCount {
        self.requested_len
    }

    #[cfg(unittest)]
    pub(crate) fn is_partial(self) -> bool {
        self.block_count() < self.requested_len
    }

    pub(crate) fn first_block_allocation(self) -> BlockAllocation {
        self.allocation.first_block_allocation()
    }
}

/// Group selection parameters of one write allocation attempt.
#[derive(Clone, Copy)]
struct GroupAttempt {
    goal: Option<FilesystemBlock>,
    group: BlockGroupNumber,
    min_len: BlockCount,
    /// Pre-clamped by the caller to the group size and the mount free total.
    expected_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FreeBlockRun {
    first_bit: u32,
    len: u32,
}

impl FreeBlockRun {
    const fn end_bit(self) -> u32 {
        self.first_bit + self.len
    }

    const fn contains(self, bit: u32) -> bool {
        self.first_bit <= bit && bit < self.end_bit()
    }
}

/// In-memory per-group free extent cache for the mballoc fast path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockGroupFreeExtentCache {
    free_runs: Vec<FreeBlockRun>,
    order_buckets: Vec<Vec<FreeBlockRun>>,
    free_blocks: u32,
}

impl BlockGroupFreeExtentCache {
    pub(crate) fn from_bitmap(
        range: crate::bitmap_allocator::BlockGroupRange,
        bitmap: &[u8],
        mut is_protected: impl FnMut(FilesystemBlock) -> bool,
    ) -> Ext4Result<Self> {
        let mut free_runs = Vec::new();
        let mut bit = 0u32;
        let mut free_blocks = 0u32;

        while bit < range.block_count() {
            let block = range.block_at(bit)?;
            if is_ext4_bitmap_bit_set(bitmap, bit)? || is_protected(block) {
                bit = bit.checked_add(1).ok_or(Ext4Error::Overflow)?;
                continue;
            }

            let first_bit = bit;
            while bit < range.block_count() {
                let block = range.block_at(bit)?;
                if is_ext4_bitmap_bit_set(bitmap, bit)? || is_protected(block) {
                    break;
                }
                bit = bit.checked_add(1).ok_or(Ext4Error::Overflow)?;
            }
            let len = bit.checked_sub(first_bit).ok_or(Ext4Error::Overflow)?;
            free_blocks = free_blocks.checked_add(len).ok_or(Ext4Error::Overflow)?;
            free_runs.push(FreeBlockRun { first_bit, len });
        }

        let mut cache = Self {
            free_runs,
            order_buckets: vec![Vec::new(); MBALLOC_ORDER_BUCKETS],
            free_blocks,
        };
        cache.rebuild_order_buckets()?;
        Ok(cache)
    }

    pub(crate) const fn free_blocks(&self) -> u32 {
        self.free_blocks
    }

    /// Length of the contiguous free run covering `bit`, measured from `bit`;
    /// 0 when `bit` is not free. The cache mirrors the bitmap, so prealloc
    /// spaces are invisible here - a surplus sampler must clip the result
    /// against the prealloc index.
    ///
    /// - `bit`: group-relative bitmap bit to measure from.
    pub(crate) fn free_run_len_from(&self, bit: u32) -> u32 {
        // free_runs stays sorted by first_bit (built ascending; splits and
        // ordered inserts keep it), so one binary search finds the only run
        // that can contain `bit`.
        self.free_runs
            .partition_point(|run| run.first_bit <= bit)
            .checked_sub(1)
            .and_then(|index| self.free_runs.get(index))
            .filter(|run| run.contains(bit))
            .map_or(0, |run| run.end_bit() - bit)
    }

    pub(crate) fn suggest_goal(
        &self,
        range: crate::bitmap_allocator::BlockGroupRange,
        goal: Option<FilesystemBlock>,
        min_len: BlockCount,
        expected_len: BlockCount,
    ) -> Ext4Result<Option<FilesystemBlock>> {
        if self.free_blocks < min_len.get() {
            return Ok(None);
        }

        let goal_candidate = if let Some(goal) = goal.filter(|goal| range.contains(*goal)) {
            let goal_bit = range.bit_index(goal)?;
            if let Some(run) = self.free_runs.iter().find(|run| run.contains(goal_bit)) {
                let available = run
                    .end_bit()
                    .checked_sub(goal_bit)
                    .ok_or(Ext4Error::Overflow)?;
                if available >= expected_len.get() {
                    return range.block_at(goal_bit).map(Some);
                }
                (available >= min_len.get()).then_some(goal_bit)
            } else {
                None
            }
        } else {
            None
        };

        let best = self.best_ordered_run(min_len.get(), expected_len.get())?;
        if let Some(run) = best.filter(|run| run.len >= expected_len.get()) {
            return range.block_at(run.first_bit).map(Some);
        }
        if let Some(goal_bit) = goal_candidate {
            return range.block_at(goal_bit).map(Some);
        }
        match best {
            Some(run) => range.block_at(run.first_bit).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn mark_allocated(
        &mut self,
        first_bit: u32,
        block_count: BlockCount,
    ) -> Ext4Result<()> {
        let len = block_count.get();
        let end_bit = first_bit.checked_add(len).ok_or(Ext4Error::Overflow)?;
        let index = self
            .free_runs
            .iter()
            .position(|run| run.first_bit <= first_bit && end_bit <= run.end_bit())
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))?;
        let run = self.free_runs[index];
        let left_len = first_bit
            .checked_sub(run.first_bit)
            .ok_or(Ext4Error::Overflow)?;
        let right_len = run
            .end_bit()
            .checked_sub(end_bit)
            .ok_or(Ext4Error::Overflow)?;

        self.free_runs.remove(index);
        if right_len != 0 {
            self.free_runs.insert(
                index,
                FreeBlockRun {
                    first_bit: end_bit,
                    len: right_len,
                },
            );
        }
        if left_len != 0 {
            self.free_runs.insert(
                index,
                FreeBlockRun {
                    first_bit: run.first_bit,
                    len: left_len,
                },
            );
        }
        self.free_blocks = self
            .free_blocks
            .checked_sub(len)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))?;
        self.rebuild_order_buckets()?;
        Ok(())
    }

    #[cfg(unittest)]
    pub(crate) fn mark_free(&mut self, bit: u32) -> Ext4Result<()> {
        self.mark_free_run(bit, 1)
    }

    pub(crate) fn mark_free_run(&mut self, bit: u32, count: u32) -> Ext4Result<()> {
        if count == 0 {
            return Err(Ext4Error::OutOfBounds);
        }
        let end = bit.checked_add(count).ok_or(Ext4Error::Overflow)?;
        let mut insert_at = 0usize;
        while insert_at < self.free_runs.len() && self.free_runs[insert_at].first_bit < bit {
            if self.free_runs[insert_at].contains(bit) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
            }
            insert_at += 1;
        }

        if self
            .free_runs
            .get(insert_at)
            .is_some_and(|right| right.first_bit < end)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        let merge_left = insert_at
            .checked_sub(1)
            .and_then(|left| self.free_runs.get(left).copied())
            .is_some_and(|left| left.end_bit() == bit);
        let merge_right = self
            .free_runs
            .get(insert_at)
            .copied()
            .is_some_and(|right| end == right.first_bit);

        match (merge_left, merge_right) {
            (true, true) => {
                let right = self.free_runs.remove(insert_at);
                let left = self
                    .free_runs
                    .get_mut(insert_at - 1)
                    .ok_or(Ext4Error::OutOfBounds)?;
                left.len = left
                    .len
                    .checked_add(count)
                    .and_then(|len| len.checked_add(right.len))
                    .ok_or(Ext4Error::Overflow)?;
            }
            (true, false) => {
                let left = self
                    .free_runs
                    .get_mut(insert_at - 1)
                    .ok_or(Ext4Error::OutOfBounds)?;
                left.len = left.len.checked_add(count).ok_or(Ext4Error::Overflow)?;
            }
            (false, true) => {
                let right = self
                    .free_runs
                    .get_mut(insert_at)
                    .ok_or(Ext4Error::OutOfBounds)?;
                right.first_bit = bit;
                right.len = right.len.checked_add(count).ok_or(Ext4Error::Overflow)?;
            }
            (false, false) => self.free_runs.insert(
                insert_at,
                FreeBlockRun {
                    first_bit: bit,
                    len: count,
                },
            ),
        }
        self.free_blocks = self
            .free_blocks
            .checked_add(count)
            .ok_or(Ext4Error::Overflow)?;
        self.rebuild_order_buckets()?;
        Ok(())
    }

    fn best_ordered_run(
        &self,
        min_len: u32,
        expected_len: u32,
    ) -> Ext4Result<Option<FreeBlockRun>> {
        let first_order = order_bucket_floor(min_len)?;
        let mut best = None;
        for order in first_order..self.order_buckets.len() {
            for run in self.order_buckets[order].iter().copied() {
                if run.len < min_len {
                    continue;
                }
                let candidate_key = (
                    run.len < expected_len,
                    run.len.abs_diff(expected_len),
                    run.first_bit,
                );
                let should_replace = best
                    .map(|current: FreeBlockRun| {
                        candidate_key
                            < (
                                current.len < expected_len,
                                current.len.abs_diff(expected_len),
                                current.first_bit,
                            )
                    })
                    .unwrap_or(true);
                if should_replace {
                    best = Some(run);
                }
            }
            if best.is_some_and(|run| run.len >= expected_len) {
                break;
            }
        }
        Ok(best)
    }

    /// Rebuild the "order buckets" of free blocks.
    fn rebuild_order_buckets(&mut self) -> Ext4Result<()> {
        // The run list is only ever produced by from_bitmap, mark_allocated,
        // and mark_free; sorted, disjoint runs whose lengths sum to the
        // cached free count are the cache's core invariant, mirroring the
        // buddy-tree consistency checks in Linux mballoc.
        debug_assert!(
            self.free_runs
                .windows(2)
                .all(|pair| pair[0].end_bit() < pair[1].first_bit),
            "free-extent cache runs are not sorted and disjoint"
        );
        debug_assert!(
            self.free_runs
                .iter()
                .try_fold(0u32, |total, run| total.checked_add(run.len))
                .is_some_and(|total| total == self.free_blocks),
            "free-extent cache run lengths disagree with free_blocks={}",
            self.free_blocks
        );
        for bucket in &mut self.order_buckets {
            bucket.clear();
        }
        for run in self.free_runs.iter().copied() {
            let order = order_bucket_floor(run.len)?;
            self.order_buckets
                .get_mut(order)
                .ok_or(Ext4Error::Overflow)?
                .push(run);
        }
        Ok(())
    }
}

fn order_bucket_floor(blocks: u32) -> Ext4Result<usize> {
    if blocks == 0 {
        return Err(Ext4Error::OutOfBounds);
    }
    usize::try_from(u32::BITS - 1 - blocks.leading_zeros()).map_err(|_| Ext4Error::Overflow)
}

impl Ext4SbInfo {
    pub(crate) fn reset_block_allocation_caches(&self) {
        reset_block_allocation_caches_inner(&mut lock(&self.allocator));
    }
}

pub(crate) fn reset_block_allocation_caches_inner(alloc: &mut AllocatorState) {
    alloc.block_free_extent_caches = vec![None; alloc.groups.len()];
}

/// Allocation-goal input for [`ensure_block_group_free_cache_inner`].
pub(crate) struct FreeCacheGoalRequest {
    pub(crate) goal: Option<FilesystemBlock>,
    pub(crate) min_len: BlockCount,
    pub(crate) expected_len: BlockCount,
}

/// Ensures the per-group free-extent cache exists, building it from `bitmap`
/// on first use. When `goal_request` is present, returns the allocation goal
/// the cache suggests for that request.
///
/// Callers must hold the allocator lock and must not invoke other
/// `Ext4SbInfo` methods that re-lock it while this runs.
pub(crate) fn ensure_block_group_free_cache_inner(
    alloc: &mut AllocatorState,
    group: BlockGroupNumber,
    range: crate::bitmap_allocator::BlockGroupRange,
    bitmap: &[u8],
    goal_request: Option<FreeCacheGoalRequest>,
    is_system_zone_block: impl Fn(FilesystemBlock) -> bool,
) -> Ext4Result<Option<FilesystemBlock>> {
    let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
    if alloc
        .block_free_extent_caches
        .get(group_index)
        .ok_or(Ext4Error::OutOfBounds)?
        .is_none()
    {
        let cache = BlockGroupFreeExtentCache::from_bitmap(range, bitmap, is_system_zone_block)?;
        let descriptor = alloc
            .groups
            .get(group_index)
            .ok_or(Ext4Error::OutOfBounds)?;
        if !descriptor.has_uninit_block_bitmap()
            && cache.free_blocks() != descriptor.free_blocks_count()
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap));
        }
        alloc.block_free_extent_caches[group_index] = Some(cache);
    }
    let cache = alloc
        .block_free_extent_caches
        .get_mut(group_index)
        .and_then(Option::as_mut)
        .ok_or(Ext4Error::OutOfBounds)?;
    match goal_request {
        Some(request) => {
            cache.suggest_goal(range, request.goal, request.min_len, request.expected_len)
        }
        None => Ok(None),
    }
}

impl Ext4SbInfo {
    /// Allocates the run for one write request, scanning groups in mballoc
    /// order and attaching no post-allocation state.
    ///
    /// - `request`: the write request; lengths in blocks.
    /// - `handle`: journal handle carrying the winning group's updates.
    ///
    /// Returns: the allocated run paired with `request.logical_start`.
    ///
    /// Errors: `NoSpace` when no group served the request; other errors
    /// propagate unchanged.
    ///
    /// Locking: caller must not hold the allocator mutex. May sleep.
    pub(crate) fn allocate_blocks_for_write(
        &self,
        request: Ext4AllocationRequest,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Ext4AllocatedRun> {
        self.allocate_blocks_for_write_with(request, handle, |_, _| {})
    }

    /// One group's allocation attempt under the allocator lock: re-checks
    /// the descriptor (the caller's peek is a stale hint), allocates, and
    /// runs the in-lock hook on success.
    ///
    /// - `alloc`: live allocator guard.
    /// - `attempt`: the group selection parameters.
    /// - `handle`: journal handle marking the run.
    /// - `after_allocated`: the in-lock hook; see
    ///   [`Self::allocate_blocks_for_write_with`].
    ///
    /// Locking: caller must hold the allocator mutex.
    fn attempt_group_allocation_locked(
        &self,
        alloc: &mut AllocatorState,
        attempt: GroupAttempt,
        handle: &mut JournalHandle<'_>,
        after_allocated: &mut impl FnMut(&mut AllocatorState, &BlockRunAllocation),
    ) -> Ext4Result<Option<BlockRunAllocation>> {
        let group = attempt.group;
        let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
        let descriptor = alloc
            .groups
            .get(group_index)
            .ok_or(Ext4Error::OutOfBounds)?;
        if !descriptor.has_uninit_block_bitmap()
            && descriptor.free_blocks_count() < attempt.min_len.get()
        {
            return Ok(None);
        }
        let group_goal = attempt.goal.filter(|block| {
            self.block_group_for_block(*block)
                .is_ok_and(|goal_group| goal_group == group)
        });
        let allocation = self.allocate_block_run_in_group_locked(
            alloc,
            group,
            group_goal,
            attempt.min_len,
            BlockCount::new(attempt.expected_len),
            handle,
        )?;
        after_allocated(alloc, &allocation);
        Ok(Some(allocation))
    }

    /// Scans groups for a write allocation, running `after_allocated` inside the
    /// same allocator-lock section as the successful allocation, so the caller
    /// can attach state that no other allocator may observe half-built; the
    /// preallocation entry registers its surplus there. Linux births a PA under
    /// the allocation's group lock for the same reason.
    ///
    /// - `request`: the write request; lengths in blocks, and
    ///   `min_len < expected_len` only when its flags allow a partial run.
    /// - `handle`: journal handle carrying the winning group's updates.
    /// - `after_allocated`: runs at most once, only on success; under the guard
    ///   it may take nothing but an owner's prealloc map leaf lock.
    ///
    /// Returns: the allocated run paired with `request.logical_start`, shorter
    /// than `expected_len` only when a partial run is allowed.
    ///
    /// Errors: `NoSpace` when no group served the request after the discard
    /// retries; other errors propagate unchanged.
    ///
    /// Worth noting: one corrupt group does not fail the write. The first
    /// `Corrupt` is remembered and returned instead of `NoSpace` only when no
    /// healthy group served; it does not block retries, which are gated on a
    /// healthy group reporting `NoSpace`.
    ///
    /// Locking: caller must not hold the allocator mutex; the discard between
    /// retry rounds takes it. Prefetch runs unlocked. May sleep.
    pub(crate) fn allocate_blocks_for_write_with(
        &self,
        request: Ext4AllocationRequest,
        handle: &mut JournalHandle<'_>,
        mut after_allocated: impl FnMut(&mut AllocatorState, &BlockRunAllocation),
    ) -> Ext4Result<Ext4AllocatedRun> {
        if self.free_blocks_count() == 0 {
            return Err(Ext4Error::NoSpace);
        }
        let start = match request.goal {
            Some(goal) => self.block_allocation_start_group(Some(goal))?,
            None => request.locality_group(),
        };
        // First bitmap corruption met while scanning, latched so it outlives the
        // retry rounds and wins over the final NoSpace.
        let mut first_corruption = None;
        // Set when any healthy group returns Err(NoSpace) from the bitmap
        // scanner — prealloc spaces may be blocking it, so a retry behind
        // a discard could help. Ok(None) (descriptor shortfall) does not
        // set this, since discarding cannot change group capacities.
        let mut healthy_nospace = false;
        let min_len = if request.flags.allows_partial() {
            request.min_len
        } else {
            request.expected_len
        };
        // The mount-wide free total does not vary per group; cache it once
        // instead of taking the allocator lock again for every scanned
        // group. It only caps the requested `expected_len`, and the actual
        // allocation re-validates free space inside its own critical
        // section, so a stale snapshot is harmless.
        let mount_free_blocks = self.free_blocks_count().min(u64::from(u32::MAX)) as u32;

        // One common reason the scan comes up empty is that prealloc spaces
        // hold the groups: the bitmap scanner skips their bits while the free
        // aggregate still counts them. So retry behind a discard, at most
        // `PREALLOC_DISCARD_SEARCH_LIMIT` searches; a discard that frees nothing
        // ends the loop. Linux: the `repeat:` path of
        // `ext4_mb_new_blocks` (fs/ext4/mballoc.c:6308-6339).
        let mut searches = 0u32;
        loop {
            for group in self.group_scan_order(start)? {
                let group_index = usize::try_from(group.get()).map_err(|_| Ext4Error::Overflow)?;
                let range = self.block_group_range(group)?;
                // Pure geometry first: skip impossible groups without any lock.
                let expected_len = request
                    .expected_len
                    .get()
                    .min(range.block_count())
                    .min(mount_free_blocks);
                if min_len.get() > range.block_count() || expected_len < min_len.get() {
                    continue;
                }
                // Cheap descriptor peek under a short lock, so the prefetch
                // below (which may hit the device) runs only for groups worth
                // trying. `min_len` is at least one block (validated at
                // request construction), so an empty group is covered by the
                // same skip.
                let worth_a_try = {
                    let alloc = lock(&self.allocator);
                    match alloc.groups.get(group_index) {
                        Some(descriptor) => {
                            descriptor.has_uninit_block_bitmap()
                                || descriptor.free_blocks_count() >= min_len.get()
                        }
                        None => return Err(Ext4Error::OutOfBounds),
                    }
                };
                if !worth_a_try {
                    continue;
                }
                // Warm the metadata cache outside the allocator lock: a
                // cold-cache device read must not serialize every allocator
                // behind the mutex. The attempt re-reads authoritatively
                // inside.
                self.prefetch_group_allocation_metadata(group)?;
                // One lock section covers the attempt and the post-allocation
                // hook, so the hook observes - and may extend - exactly the
                // state the allocation published.
                let attempt = {
                    let mut alloc = lock(&self.allocator);
                    self.attempt_group_allocation_locked(
                        &mut alloc,
                        GroupAttempt {
                            goal: request.goal,
                            group,
                            min_len,
                            expected_len,
                        },
                        handle,
                        &mut after_allocated,
                    )
                };
                match attempt {
                    Ok(Some(allocation)) => {
                        return Ok(Ext4AllocatedRun::new(
                            request.logical_start,
                            request.expected_len,
                            allocation,
                        ));
                    }
                    Ok(None) => continue,
                    Err(Ext4Error::NoSpace) => {
                        healthy_nospace = true;
                        continue;
                    }
                    // A healthy group may still serve, so keep scanning and
                    // remember the first hit. kext4 has no persistent
                    // `BBITMAP_CORRUPT` buddy flag, so each scan re-detects it.
                    Err(error @ Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap)) => {
                        first_corruption.get_or_insert(error);
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }

            searches += 1;
            // Only retry when a healthy group reported NoSpace.
            if healthy_nospace
                && searches < PREALLOC_DISCARD_SEARCH_LIMIT
                && self.discard_preallocs_for_space(u64::from(min_len.get())) > 0
            {
                continue;
            }
            break;
        }

        Err(first_corruption.unwrap_or(Ext4Error::NoSpace))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/mballoc/tests.rs");

#[cfg(unittest)]
unittest::path_mod_test!(component_tests, "../ut-tests/mballoc/component_tests.rs");
