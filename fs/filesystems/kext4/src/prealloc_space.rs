// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! In-memory bookkeeping for writeback speculative preallocation.
//!
//! Linux analog: `struct ext4_prealloc_space` (mballoc). A prealloc space
//! records the surplus of a normalized writeback allocation window without
//! marking its blocks in the bitmap; the range reaches the bitmap only when
//! a later writeback run consumes the space, inside that run's journal
//! handle, so crash or unmount drops spaces with zero disk residue. Linux
//! marks the surplus in the in-core buddy and must sync those marks with the
//! PA lifecycle; here the two-sided index below is the only record.
//!
//! # Structure: one prealloc space, two local views
//!
//! Linux keeps one PA object on two lists (inode `i_prealloc_node` rbtree
//! for consumption, group `bb_prealloc_list` for enumeration and discard),
//! with exclusion riding on the buddy marks. Same split here:
//!
//! - Inode side: [`InodePreallocMap`], keyed by logical start block;
//!   consumption is an O(log n) lookup local to one file.
//! - Group side: [`AllocatorState::preallocs_by_group`], keyed by `(group,
//!   first physical block)`, each twin holding a strong owner reference (the
//!   `pa_inode` analog). Group scans exclude preallocated ranges through a
//!   per-attempt snapshot of
//!   [`AllocatorState::prealloc_bit_ranges_in_group`], taken under the
//!   allocator lock and rejected by the low-level bitmap scanner's combined
//!   `is_protected` closure - the role the buddy marks play in Linux. The
//!   ENOSPC retry valve
//!   ([`Ext4SbInfo::discard_preallocs_for_space`], the
//!   `ext4_mb_discard_preallocations` analog) enumerates it group-locally.
//!
//! Both sides mutate only together, inside one allocator-mutex section: a
//! stale one-sided entry would protect nothing and trade on a dead promise.
//!
//! Only per-inode spaces exist: Linux's locality-group PAs (`MB_GROUP_PA`,
//! the fallback when inode PAs miss) have no counterpart here. kext4 has no
//! locality-group producer, and the writeback normalizer never inflates a
//! request below four blocks, so small writes register no space at all.
//!
//! # Identity and lifecycle
//!
//! A space is owned by its inode object: born inside the allocation lock
//! section that produced its backing run
//! ([`Ext4SbInfo::allocate_blocks_for_write_with_prealloc`]; Linux births
//! PAs under the allocation's own group lock too), consumed atomically
//! ([`Ext4SbInfo::consume_prealloc_for_write`]), cut back by shrink truncate
//! ([`Ext4SbInfo::clip_preallocs_from_logical`]), and dropped at the last
//! writable close and at eviction ([`Ext4SbInfo::release_preallocs`], the
//! `ext4_discard_preallocations` sites).
//!
//! The free aggregate never sees spaces: [`Ext4SbInfo::free_blocks_count`] is
//! the raw on-disk count, so an unconsumed surplus still reads as free
//! (Linux statfs parity).
//!
//! # Locking
//!
//! Lock order: *ext4 rwlock -> allocator mutex -> prealloc map lock*. The map
//! lock is a module-private leaf, never the inode-state lock, so prealloc
//! operations cannot form an ABBA cycle with inode-state users; discard
//! locks foreign owners' maps in the same order. `update` closures run under
//! the allocator guard and must acquire no further lock. The allocator mutex
//! stays held across consumption, which closes the window between selecting a
//! space and marking its bitmap bits.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::{
    BlockCount, BlockGroupNumber, FilesystemBlock, InodeNumber, LogicalBlock, PhysicalBlock,
    bitmap_allocator::{BlockGroupRange, BlockRunAllocation},
    error::{Ext4Error, Ext4Result},
    inode::Ext4Inode,
    jbd2::JournalHandle,
    mballoc::{Ext4AllocatedRun, Ext4AllocationRequest},
    superblock::{AllocatorState, Ext4SbInfo, lock},
    sync::{Mutex, mutex_lock},
};

/// Allocation searches the `mballoc` write scan may run before giving up with
/// `NoSpace`. A discard may precede each search after the first, so it needs a
/// value of at least 2 to do anything at all. The single-group `balloc` entry
/// does not retry: its group-unaware discard cannot guarantee reaching the
/// requested group. Linux: `++retries < 3` in `ext4_mb_new_blocks`
/// (fs/ext4/mballoc.c:6337).
pub(crate) const PREALLOC_DISCARD_SEARCH_LIMIT: u32 = 3;

/// Debug-only conservation check: the counter must equal the sum of the
/// group-index lengths. Every mutation ends with it.
///
/// Locking: caller must hold the allocator mutex.
macro_rules! debug_assert_prealloc_conservation {
    ($state:expr) => {
        debug_assert_eq!(
            $state.preallocated_blocks,
            $state
                .preallocs_by_group
                .values()
                .map(|twin| u64::from(twin.phys_len.get()))
                .sum::<u64>(),
            "preallocated counter drifted from the group index"
        );
    };
}

/// Cross-checks an inode-side entry against its group-side twin. Both sides
/// mutate together under the allocator mutex, so drift is a bookkeeping bug:
/// debug builds abort, release builds log and continue - the analog of
/// Linux's `free != pa_free` check in `ext4_mb_release_inode_pa`.
///
/// - `owner`: inode number, for the report.
/// - `space`: the inode-side entry.
/// - `twin`: the group-side twin, `None` when missing.
fn check_prealloc_twin(
    owner: InodeNumber,
    space: &PreallocSpace,
    twin: Option<&GroupPreallocSpace>,
) {
    let matched = twin.is_some_and(|twin| {
        twin.phys_len == space.phys_len && twin.logical_start == space.logical_start
    });
    debug_assert!(
        matched,
        "prealloc twin drift: inode {owner:?} entry {space:?} vs twin {:?}",
        twin.map(|twin| (twin.logical_start, twin.phys_len)),
    );
    if !matched {
        error!(
            "KExt4 prealloc twin drift: inode {:?} entry {:?} vs group twin {:?}",
            owner,
            space,
            twin.map(|twin| (twin.logical_start, twin.phys_len)),
        );
    }
}

/// One unconsumed speculative range: a contiguous physical run backing an
/// equal-length logical range 1:1, so the logical length needs no field.
/// Inode-side entry; the group side carries a [`GroupPreallocSpace`] twin.
/// Linux counterpart: `ext4_prealloc_space` (`pa_lstart`/`pa_pstart`/`pa_len`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreallocSpace {
    /// Block group of the run. A space never spans two groups, which is what
    /// lets the group index key entries by group.
    pub(crate) group: BlockGroupNumber,
    pub(crate) logical_start: LogicalBlock,
    pub(crate) phys_start: FilesystemBlock,
    pub(crate) phys_len: BlockCount,
}

impl PreallocSpace {
    /// One past the last file block covered. Stored spaces cannot overflow
    /// (validated at reservation; derived spaces stay inside their origin);
    /// a saturated end over-protects rather than under-protects.
    fn logical_end(self) -> u64 {
        let end = self
            .logical_start
            .get()
            .checked_add(u64::from(self.phys_len.get()));
        debug_assert!(
            end.is_some(),
            "stored prealloc space logical end overflows u64"
        );
        end.unwrap_or(u64::MAX)
    }
}

/// Group-side twin of an inode's prealloc space: the physical record that
/// makes the space visible to group scans and discard without walking
/// inodes. Mutated only together with the inode-side entry, in one
/// allocator-mutex section. Linux counterpart: the same PA object on
/// `bb_prealloc_list`.
pub(crate) struct GroupPreallocSpace {
    /// Logical start inside the owner's file - the key of the inode-side
    /// entry, so a group-driven discard finds it in O(log n).
    pub(crate) logical_start: LogicalBlock,
    pub(crate) phys_len: BlockCount,
    /// Owner. Strong reference: the twin keeps its owner alive and the
    /// close/evict hooks guarantee the twin dies first, so a missed hook
    /// leaks memory instead of dangling (Linux's `pa_inode` is raw).
    pub(crate) owner: Arc<Ext4Inode>,
}

/// Group-index key: group + first physical block. Live spaces are pairwise
/// disjoint, so this identifies one uniquely.
pub(crate) type GroupPreallocKey = (BlockGroupNumber, FilesystemBlock);

/// One inode's prealloc spaces and their dedicated leaf lock - the Linux
/// `i_prealloc_node` + `i_prealloc_lock` pair, kept out of the inode-state
/// lock as Linux keeps it out of `i_data_sem`.
///
/// # Locking
///
/// Leaf lock, acquired by this module's prealloc operations (the
/// `Ext4Inode::prealloc_map()` accessor exists for them) with the allocator
/// mutex already held: *allocator mutex -> prealloc map lock*. The only
/// exception is [`Self::is_empty`], a read-only hint that takes the leaf lock
/// alone and releases it before returning, so it never holds this lock while
/// acquiring another and adds no wait edge. The guard never escapes
/// [`Self::update`], and the closure must acquire no other lock. With no
/// outgoing lock edge the map lock cannot take part in a cycle - the module's
/// lock order rests on this, not on crate-wide caller discipline.
pub(crate) struct InodePreallocMap {
    map: Mutex<BTreeMap<LogicalBlock, PreallocSpace>>,
}

impl InodePreallocMap {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(BTreeMap::new()),
        }
    }

    /// Runs `f` over the map under the leaf lock.
    ///
    /// - `f`: receives the logical map; must acquire no lock (see the type
    ///   docs). Its result is passed through.
    pub(crate) fn update<R>(
        &self,
        f: impl FnOnce(&mut BTreeMap<LogicalBlock, PreallocSpace>) -> R,
    ) -> R {
        let mut map = mutex_lock(&self.map);
        f(&mut map)
    }

    /// Reports whether the map holds no spaces, taking only the leaf lock.
    ///
    /// A hot-path hint: an entry may appear right after it returns `true`, so
    /// callers may only skip releases that a later close/eviction owes anyway.
    /// Taking the leaf lock alone is safe because it is released before any
    /// other lock is acquired, so it cannot close a wait cycle.
    pub(crate) fn is_empty(&self) -> bool {
        mutex_lock(&self.map).is_empty()
    }
}

/// The physical range handed to a consuming writeback run. Linux
/// counterpart: the `ac->ac_b_ex` window `ext4_mb_use_preallocated` hands
/// over for bitmap marking and extent insertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConsumedPrealloc {
    pub(crate) phys_start: FilesystemBlock,
    pub(crate) phys_len: BlockCount,
}

/// A consumption in flight: what the space was, what the run received, and
/// the owner - everything a restore needs to undo the take on both sides.
struct TakenPrealloc {
    space: PreallocSpace,
    consumed: ConsumedPrealloc,
    /// One past the last logical block the run received; a restored span
    /// ends here.
    consumed_end: u64,
    owner: Arc<Ext4Inode>,
}

/// How [`reserve_in_inode_map`] settled a candidate.
enum PreallocInsertion {
    /// Logically overlaps an existing space; nothing was stored.
    Rejected,
    /// Fused with an adjacent predecessor; reports the predecessor's
    /// physical start, whose twin absorbs the added length.
    Merged {
        pred_key: LogicalBlock,
        pred_phys_start: FilesystemBlock,
        pred_phys_len: BlockCount,
    },
    /// Stored as a new entry; needs a new group twin.
    Inserted,
}

/// Inode-side half of a registration: validates against the file's existing
/// spaces, then stores or fuses with an adjacent predecessor.
///
/// - `preallocs`: the owner's logical map, under its map lock.
/// - `space`: the candidate.
/// - `logical_end`: the candidate's precomputed logical end.
///
/// Physical overlaps (including cross-file) are checked group-side before
/// this runs. A *logical* overlap is settled per caller: the sampler treats
/// [`PreallocInsertion::Rejected`] as an ordinary "no preallocation this
/// time"; restore treats it as an invariant breach.
fn reserve_in_inode_map(
    preallocs: &mut BTreeMap<LogicalBlock, PreallocSpace>,
    space: PreallocSpace,
    logical_end: u64,
) -> PreallocInsertion {
    // Overlap with a successor whose start lies inside the candidate's
    // window must be caught before any merge: merging extends the
    // predecessor rightward and could otherwise hide the overlap behind
    // the merged entry's key shift.
    if preallocs
        .range(space.logical_start..)
        .next()
        .is_some_and(|(&next_start, _)| next_start.get() < logical_end)
    {
        return PreallocInsertion::Rejected;
    }
    if let Some((&pred_start, &pred)) = preallocs.range(..=space.logical_start).next_back() {
        // A predecessor reaching into or past the candidate's start overlaps
        // it; the same-key case is caught too (a stored space is nonzero).
        if pred.logical_end() > space.logical_start.get() {
            return PreallocInsertion::Rejected;
        }
        // Fuse with a predecessor adjacent on both keys, so an appending
        // file keeps one space instead of one per window. A merged length
        // that would overflow u32 leaves the pair apart.
        if pred.group == space.group
            && pred.logical_end() == space.logical_start.get()
            && pred
                .phys_start
                .get()
                .checked_add(u64::from(pred.phys_len.get()))
                == Some(space.phys_start.get())
            && let Some(sum) = pred.phys_len.get().checked_add(space.phys_len.get())
        {
            let merged = PreallocSpace {
                logical_start: pred_start,
                phys_start: pred.phys_start,
                phys_len: BlockCount::new(sum),
                ..space
            };
            preallocs.insert(pred_start, merged);
            return PreallocInsertion::Merged {
                pred_key: pred_start,
                pred_phys_start: pred.phys_start,
                pred_phys_len: pred.phys_len,
            };
        }
    }
    preallocs.insert(space.logical_start, space);
    PreallocInsertion::Inserted
}

/// The end bit of the range of `ranges` that covers `bit`, or `None` when no
/// range does. One binary search suffices because
/// [`AllocatorState::prealloc_bit_ranges_in_group`] yields the group's runs
/// ascending by start: the scanner's protection predicate and the caller's
/// goal advance share this single definition of the half-open shape.
///
/// - `ranges`: group-relative `[start_bit, end_bit)` runs, ascending.
/// - `bit`: the bitmap bit to locate.
pub(crate) fn range_end_containing(ranges: &[(u32, u32)], bit: u32) -> Option<u32> {
    ranges
        .partition_point(|&(start, _)| start <= bit)
        .checked_sub(1)
        .and_then(|index| ranges.get(index))
        .filter(|&&(_, end)| bit < end)
        .map(|&(_, end)| end)
}

/// Whether `bit` is claimed by any of the group's prealloc runs, i.e. the
/// scanner must treat it like a system-zone block: free on the bitmap, yet
/// not allocatable.
///
/// - `ranges`: group-relative `[start_bit, end_bit)` runs, ascending.
/// - `bit`: the bitmap bit to test.
pub(crate) fn bit_in_ranges(ranges: &[(u32, u32)], bit: u32) -> bool {
    range_end_containing(ranges, bit).is_some()
}

impl AllocatorState {
    /// Stores a new group-side twin.
    ///
    /// - `space`: the entry to file; its window must be overlap-free and
    ///   single-group (checked at the entry points).
    /// - `owner`: strong owner reference stored in the twin.
    ///
    /// A key collision asserts and logs; in release the counter is adjusted
    /// for the replaced twin so it still equals the index sum.
    ///
    /// Locking: caller must hold the allocator mutex.
    fn insert_group_prealloc(&mut self, space: PreallocSpace, owner: Arc<Ext4Inode>) {
        let previous = self.preallocs_by_group.insert(
            (space.group, space.phys_start),
            GroupPreallocSpace {
                logical_start: space.logical_start,
                phys_len: space.phys_len,
                owner,
            },
        );
        if let Some(previous) = previous {
            // Unreachable: entries check physical overlap first, and
            // consume/restore only re-file spans they just vacated. Keep the
            // ledger honest: the caller adds the new length while the
            // replaced twin's is gone, so deduct it here.
            debug_assert!(
                false,
                "prealloc space physical double-booking: group {:?} at {:?}",
                space.group, space.phys_start,
            );
            error!(
                "KExt4 prealloc double-booking: group {:?} at {:?} replaced a live twin (logical \
                 {}, len {})",
                space.group, space.phys_start, previous.logical_start, previous.phys_len,
            );
            self.preallocated_blocks -= u64::from(previous.phys_len.get());
        }
    }

    /// Whether `[phys_start, phys_end)` overlaps any live space of `group`.
    /// One group's entries are disjoint and sorted by physical start, so only
    /// the window's immediate index neighbors can overlap it.
    ///
    /// - `group`: the group to probe.
    /// - `phys_start`: first block of the window.
    /// - `phys_end`: one past the last block (overflow pre-checked).
    ///
    /// Locking: caller must hold the allocator mutex.
    fn group_prealloc_overlaps(
        &self,
        group: BlockGroupNumber,
        phys_start: FilesystemBlock,
        phys_end: u64,
    ) -> bool {
        // The predecessor may belong to an earlier group; stored twins never
        // overflow (their physical end was checked at reservation).
        if self
            .preallocs_by_group
            .range(..(group, phys_start))
            .next_back()
            .is_some_and(|(&(prev_group, prev_start), prev)| {
                prev_group == group
                    && prev_start.get() + u64::from(prev.phys_len.get()) > phys_start.get()
            })
        {
            return true;
        }
        self.preallocs_by_group
            .range((group, phys_start)..)
            .next()
            .is_some_and(|(&(next_group, next_start), _)| {
                next_group == group && next_start.get() < phys_end
            })
    }

    /// Lists the bit ranges of one group held by prealloc spaces. The blocks
    /// are not marked in the bitmap yet, so scanners must skip these bits
    /// like the system zone.
    ///
    /// - `range`: the block group to clip all spaces against.
    ///
    /// Returns: `[start_bit, end_bit)` pairs clipped to the group, ascending,
    /// touching pairs merged - the sorted-zones shape system-zone callers
    /// use. The index iterates in physical order, so one walk collects and
    /// merges without a sort. Empty when no space touches the group.
    ///
    /// Locking: caller must hold the allocator mutex; the list cannot change
    /// while it is held.
    pub(crate) fn prealloc_bit_ranges_in_group(&self, range: BlockGroupRange) -> Vec<(u32, u32)> {
        let Ok(group_first) = range.block_at(0) else {
            return Vec::new();
        };
        let group_first = group_first.get();
        let group_end = group_first + u64::from(range.block_count());
        let group = range.group();
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (&(entry_group, phys_start), twin) in self
            .preallocs_by_group
            .range((group, FilesystemBlock::new(0))..)
        {
            if entry_group != group {
                break;
            }
            let Some(space_end) = phys_start.get().checked_add(u64::from(twin.phys_len.get()))
            else {
                continue;
            };
            let start = phys_start.get().max(group_first);
            let end = space_end.min(group_end);
            if start >= end {
                continue;
            }
            let Ok(start_bit) = range.bit_index(FilesystemBlock::new(start)) else {
                continue;
            };
            let Ok(end_bit) = range.bit_index(FilesystemBlock::new(end - 1)) else {
                continue;
            };
            runs.push((start_bit, end_bit + 1));
        }
        // Collected in ascending physical order; disjoint spaces clip to
        // runs that only touch or separate, so compacting in place merges
        // the touching pairs.
        let mut write = 0;
        for read in 0..runs.len() {
            let (start, end) = runs[read];
            if write > 0 && start <= runs[write - 1].1 {
                if end > runs[write - 1].1 {
                    runs[write - 1].1 = end;
                }
            } else {
                runs[write] = (start, end);
                write += 1;
            }
        }
        runs.truncate(write);
        runs
    }
}

impl Ext4SbInfo {
    /// Birth entry: allocates the write run's blocks and, inside the *same*
    /// allocator-lock section, samples and registers the trailing free run
    /// as this inode's prealloc space. Linux counterpart: `ext4_mb_new_blocks`,
    /// which births the PA (`ext4_mb_new_inode_pa`) under the allocation's
    /// own group-lock hold. Folding registration into the allocation is what
    /// makes the promise sound: a separate entry would leave the surplus
    /// free and unprotected between the two lock sections, and a concurrent
    /// allocator could turn it into a dead promise.
    ///
    /// - `request`: the write run's allocation request; the requested run is
    ///   marked on the bitmap as usual.
    /// - `inode`: surplus owner; the strong reference goes into the group
    ///   twin (obtained e.g. via `VfsInode::downcast`).
    /// - `surplus_budget`: max trailing blocks to set aside (normalized
    ///   window minus request, clamped by the caller's remaining budget);
    ///   0 disables registration.
    /// - `handle`: journal handle of the write run.
    ///
    /// Returns: the allocation plus how many trailing blocks were registered,
    /// so the caller only spends its budget on what landed. Registration is
    /// best-effort like Linux's PA creation: blocked, too short or logically
    /// overlapping surplus yields 0 and the write proceeds without it.
    ///
    /// Locking: takes the allocator mutex internally (through the allocation
    /// core); the map leaf lock is taken and released inside the same
    /// section, so the caller needs no lock precondition.
    #[allow(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: the wiring commit calls this birth entry; \
                  expect is unreliable under --cfg unittest (tests exercise this)"
    )]
    pub(crate) fn allocate_blocks_for_write_with_prealloc(
        &self,
        request: Ext4AllocationRequest,
        inode: &Arc<Ext4Inode>,
        surplus_budget: u32,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<(Ext4AllocatedRun, u32)> {
        let mut registered = 0u32;
        let run = self.allocate_blocks_for_write_with(request, handle, |alloc, allocation| {
            let surplus_logical_start = LogicalBlock::new(
                request
                    .logical_start()
                    .get()
                    .saturating_add(u64::from(allocation.block_count().get())),
            );
            registered = self.register_prealloc_surplus_locked(
                alloc,
                inode,
                surplus_logical_start,
                allocation,
                surplus_budget,
            );
        })?;
        Ok((run, registered))
    }

    /// Samples and registers the trailing surplus of a just-published
    /// allocation; the allocation core's in-lock hook. Three clips bound the
    /// sample: the free-extent cache's run trailing the allocation ("not
    /// dirtied" - the cache mirrors the bitmap), the next prealloc space of
    /// the group ("not preallocated" - spaces are invisible to the cache, so this
    /// queries the index directly), and the caller's budget. Best-effort:
    /// any unexpected state yields 0, never an error - the allocation is
    /// already published.
    ///
    /// - `alloc`: live allocator guard.
    /// - `inode`: surplus owner.
    /// - `surplus_logical_start`: file-logical block right past the run.
    /// - `allocation`: the just-published run.
    /// - `budget`: caller's surplus cap in blocks.
    ///
    /// Returns: the registered length (0 = none).
    ///
    /// Locking: caller holds the allocator mutex.
    fn register_prealloc_surplus_locked(
        &self,
        alloc: &mut AllocatorState,
        inode: &Arc<Ext4Inode>,
        surplus_logical_start: LogicalBlock,
        allocation: &BlockRunAllocation,
        budget: u32,
    ) -> u32 {
        if budget == 0 {
            return 0;
        }
        let group = allocation.group();
        let Some(end_bit) = allocation
            .first_bitmap_bit()
            .checked_add(allocation.block_count().get())
        else {
            return 0;
        };
        let Ok(range) = self.block_group_range(group) else {
            return 0;
        };
        let Ok(group_index) = usize::try_from(group.get()) else {
            return 0;
        };
        // The allocation core rebuilt and stored this group's cache in this
        // same lock section, so it is fresh; a miss is best-effort grounds
        // to skip the surplus.
        let Some(cache) = alloc
            .block_free_extent_caches
            .get(group_index)
            .and_then(Option::as_ref)
        else {
            return 0;
        };
        let free_len = cache.free_run_len_from(end_bit);
        if free_len == 0 {
            return 0;
        }
        let Ok(phys_start) = range.block_at(end_bit) else {
            return 0;
        };
        // The cache is blind to prealloc spaces: stop the sample at the next
        // space of this group, queried straight from the index. The
        // allocation never overlaps a space (the scan excludes preallocated
        // bits), so one can only start at or after `phys_start`; same-group
        // distances fit u32, and the fallback leaves the sample unclipped.
        let space_clip = alloc
            .preallocs_by_group
            .range((group, phys_start)..)
            .next()
            .filter(|&(&(entry_group, _), _)| entry_group == group)
            .map_or(free_len, |(&(_, next_phys), _)| {
                u32::try_from(next_phys.get() - phys_start.get()).unwrap_or(free_len)
            });
        let len = free_len.min(budget).min(space_clip);
        if len == 0 {
            return 0;
        }
        self.reserve_prealloc_locked(
            alloc,
            inode,
            surplus_logical_start,
            phys_start,
            BlockCount::new(len),
        )
    }

    /// Validates one sampled window and files it on both sides. Private by
    /// design: a space may only be born inside the allocation lock section
    /// that produced its backing run
    /// ([`Self::allocate_blocks_for_write_with_prealloc`]). The sampler's
    /// window is structurally single-group, bitmap-free and space-disjoint,
    /// so the cross-group and physical-overlap rejections are depth guards
    /// (assert + log); a logical overlap is an ordinary give-up.
    ///
    /// - `alloc`: live allocator guard.
    /// - `inode`: owner of the space.
    /// - `logical_start`: first file block of the window.
    /// - `phys_start`: first physical block of the backing run.
    /// - `phys_len`: window length in blocks.
    ///
    /// Returns: the accepted length; 0 when rejected, with neither side
    /// changed.
    ///
    /// Locking: caller holds the allocator mutex; the map leaf lock is taken
    /// and released internally.
    fn reserve_prealloc_locked(
        &self,
        alloc: &mut AllocatorState,
        inode: &Arc<Ext4Inode>,
        logical_start: LogicalBlock,
        phys_start: FilesystemBlock,
        phys_len: BlockCount,
    ) -> u32 {
        if phys_len.get() == 0 {
            return 0;
        }
        let Some(logical_end) = logical_start.get().checked_add(u64::from(phys_len.get())) else {
            return 0;
        };
        let Some(phys_end) = phys_start.get().checked_add(u64::from(phys_len.get())) else {
            return 0;
        };
        let Ok(group) = self.block_group_for_block(phys_start) else {
            return 0;
        };
        // Single-group invariant: the index keys a space only by its `group`,
        // so a spanning window would leave the tail group unprotected and let
        // a normal allocation steal the blocks promised there. Contiguity plus
        // monotonic block-to-group numbering make an end-block check suffice.
        let single_group = self
            .block_group_for_block(FilesystemBlock::new(phys_end - 1))
            .is_ok_and(|tail| tail == group);
        if !single_group {
            debug_assert!(
                false,
                "prealloc space window spans block groups: {:?} + {}",
                phys_start, phys_len,
            );
            warn!(
                "KExt4 prealloc window spans block groups: {:?} + {}",
                phys_start, phys_len,
            );
            return 0;
        }
        if alloc.group_prealloc_overlaps(group, phys_start, phys_end) {
            debug_assert!(
                false,
                "prealloc space physical double-booking: group {:?}, {:?} + {}",
                group, phys_start, phys_len,
            );
            warn!(
                "KExt4 prealloc physical double-booking: group {:?}, {:?} + {}",
                group, phys_start, phys_len,
            );
            return 0;
        }
        let space = PreallocSpace {
            group,
            logical_start,
            phys_start,
            phys_len,
        };
        let outcome = inode
            .prealloc_map()
            .update(|preallocs| reserve_in_inode_map(preallocs, space, logical_end));
        match outcome {
            // Ordinary "no preallocation this time": an earlier run may
            // already cover the logical range. Linux likewise proceeds
            // without creating a PA.
            PreallocInsertion::Rejected => return 0,
            PreallocInsertion::Merged {
                pred_key,
                pred_phys_start,
                pred_phys_len,
            } => {
                // Both sides mutate together under this mutex.  If the
                // group-side twin is gone, the inode-side merge that
                // reserve_in_inode_map already wrote must be rolled back:
                // a one-sided entry would protect a range nothing enforces.
                let Some(twin) = alloc.preallocs_by_group.get_mut(&(group, pred_phys_start)) else {
                    inode.prealloc_map().update(|preallocs| {
                        if let Some(merged) = preallocs.remove(&pred_key) {
                            preallocs.insert(
                                pred_key,
                                PreallocSpace {
                                    phys_len: pred_phys_len,
                                    ..merged
                                },
                            );
                        }
                    });
                    debug_assert!(
                        false,
                        "merge predecessor at ({:?}, {:?}) lost its group twin",
                        group, pred_phys_start,
                    );
                    warn!(
                        "KExt4 prealloc merge predecessor at ({:?}, {:?}) lost its group twin; \
                         dropping the window",
                        group, pred_phys_start,
                    );
                    return 0;
                };
                // The merge itself checked the summed length.
                twin.phys_len = BlockCount::new(twin.phys_len.get().saturating_add(phys_len.get()));
            }
            PreallocInsertion::Inserted => alloc.insert_group_prealloc(space, inode.clone()),
        }
        alloc.preallocated_blocks += u64::from(phys_len.get());
        debug_assert_prealloc_conservation!(alloc);
        // Spaces are disjoint ranges of still-free blocks (scans skip
        // preallocated bits), so the total can never exceed the free
        // aggregate; exceeding it means the exclusion was bypassed.
        debug_assert!(
            alloc.preallocated_blocks <= alloc.free_blocks_count,
            "preallocated {} exceeds free aggregate {}",
            alloc.preallocated_blocks,
            alloc.free_blocks_count,
        );
        phys_len.get()
    }

    /// Write-path entry: turns a prealloc space into real blocks in one
    /// atomic step - under a single allocator-lock section it takes the
    /// promised range (detaching both sides) and marks it through the normal
    /// allocation path, so exclusion cannot lapse between selection and the
    /// on-disk mark (Linux: `ext4_mb_use_preallocated` through
    /// `ext4_mb_mark_diskspace_used`; a two-step take-then-allocate would
    /// open a TOCTOU window).
    ///
    /// - `inode`: the writing file.
    /// - `logical_start`: first file block the run needs.
    /// - `logical_len`: how many file blocks the run needs.
    /// - `handle`: journal handle of the consuming run.
    ///
    /// Returns: the marked physical range, or `None` when the request is
    /// empty, no space contains its first block, or the run could not be
    /// secured - on `None`/error nothing is consumed (the span is restored
    /// on both sides): a lost hint, never a lost block. At most one space is
    /// consumed per call, like Linux.
    ///
    /// Locking: takes the allocator mutex and holds it across the whole
    /// operation; the map leaf lock is taken and released inside.
    #[allow(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: the wiring commit calls this consume entry; \
                  expect is unreliable under --cfg unittest (tests exercise this)"
    )]
    pub(crate) fn consume_prealloc_for_write(
        &self,
        inode: &Ext4Inode,
        logical_start: LogicalBlock,
        logical_len: u64,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<Option<(PhysicalBlock, BlockCount)>> {
        // An empty request has nothing to consume; answering it here keeps
        // the space intact instead of splitting it around a zero-length
        // middle. An overflowing bound falls back to normal allocation.
        if logical_len == 0 {
            return Ok(None);
        }
        let Some(request_end) = logical_start.get().checked_add(logical_len) else {
            return Ok(None);
        };
        let mut alloc = lock(&self.allocator);
        let taken = inode.prealloc_map().update(|preallocs| {
            // Hit lookup: the space containing the request's first block.
            // Linux requires the same (`ext4_mb_use_preallocated` keeps a pa
            // only when `pa_lstart <= request < pa end`); a later space must
            // not be taken or the physical translation below would hand out
            // the wrong blocks.
            let (&key, &space) = preallocs
                .range(..=logical_start)
                .next_back()
                .filter(|(_, c)| logical_start.get() < c.logical_end())?;
            // Intersection: the consumed span runs from the request start to
            // whichever end comes first, covering prefix, interior and whole
            // consumption without branching; the kept tail shifts on both
            // keys by the 1:1 rule established at reservation.
            let consumed_end = request_end.min(space.logical_end());
            let Ok(consumed_len) = u32::try_from(consumed_end - logical_start.get()) else {
                return None;
            };
            let consumed = ConsumedPrealloc {
                phys_start: FilesystemBlock::new(
                    space.phys_start.get() + (logical_start.get() - space.logical_start.get()),
                ),
                phys_len: BlockCount::new(consumed_len),
            };
            let head_len = logical_start.get() - space.logical_start.get();
            let tail_len = space.logical_end() - consumed_end;
            // Detach the twin before mutating anything: bailing on a missing
            // twin leaves both sides untouched instead of consuming against
            // a promise nothing protects.
            let Some(twin) = alloc
                .preallocs_by_group
                .remove(&(space.group, space.phys_start))
            else {
                debug_assert!(false, "prealloc space {:?} lost its group twin", space);
                error!(
                    "KExt4 prealloc space {:?} of inode {:?} lost its group twin; consumption \
                     refused",
                    space,
                    inode.number(),
                );
                return None;
            };
            check_prealloc_twin(inode.number(), &space, Some(&twin));
            // Split and re-store on both sides; head/tail lengths are capped
            // by `phys_len <= u32::MAX`, so the casts cannot lose value.
            debug_assert!(head_len <= u64::from(u32::MAX) && tail_len <= u64::from(u32::MAX));
            preallocs.remove(&key);
            if head_len > 0 {
                let head = PreallocSpace {
                    phys_len: BlockCount::new(head_len as u32),
                    ..space
                };
                preallocs.insert(head.logical_start, head);
                alloc.insert_group_prealloc(head, twin.owner.clone());
            }
            if tail_len > 0 {
                let tail = PreallocSpace {
                    logical_start: LogicalBlock::new(consumed_end),
                    phys_start: FilesystemBlock::new(
                        consumed.phys_start.get() + u64::from(consumed_len),
                    ),
                    phys_len: BlockCount::new(tail_len as u32),
                    ..space
                };
                preallocs.insert(tail.logical_start, tail);
                alloc.insert_group_prealloc(tail, twin.owner.clone());
            }
            Some(TakenPrealloc {
                space,
                consumed,
                consumed_end,
                owner: twin.owner,
            })
        });
        let Some(taken) = taken else {
            return Ok(None);
        };
        // Counter balancing: subtract exactly the delivered span; head and
        // tail stay on the books.
        alloc.preallocated_blocks -= u64::from(taken.consumed.phys_len.get());
        debug_assert_prealloc_conservation!(alloc);
        let count = taken.consumed.phys_len;
        // The span lies inside the space, which is single-group by birth.
        let group = taken.space.group;
        // Pin the goal to the preallocated start: still free and, with its
        // twin just detached, no longer excluded from this scan, so the scan
        // normally returns that exact run. A fallback run elsewhere stays
        // valid - the extent table records the physical truth either way.
        match self.allocate_block_run_in_group_locked(
            &mut alloc,
            group,
            Some(taken.consumed.phys_start),
            count,
            count,
            handle,
        ) {
            Ok(allocation) => Ok(Some((allocation.first_block(), allocation.block_count()))),
            Err(Ext4Error::NoSpace) => {
                self.restore_taken_prealloc(&mut alloc, inode, logical_start, &taken);
                Ok(None)
            }
            Err(error) => {
                self.restore_taken_prealloc(&mut alloc, inode, logical_start, &taken);
                Err(error)
            }
        }
    }

    /// Puts a taken span back on both sides after its mark could not be
    /// secured, so a fallback is a pure lost hint and never a lost promise.
    /// Re-inserting at the request's start fuses with the kept head; the
    /// tail stays a separate adjacent entry.
    ///
    /// - `alloc`: live allocator guard.
    /// - `inode`: owner.
    /// - `logical_start`: logical start of the consumed span.
    /// - `taken`: the consumption snapshot to undo.
    ///
    /// Locking: caller must hold the allocator mutex.
    fn restore_taken_prealloc(
        &self,
        alloc: &mut AllocatorState,
        inode: &Ext4Inode,
        logical_start: LogicalBlock,
        taken: &TakenPrealloc,
    ) {
        // The span's group is the space's group: single-group by birth.
        let group = taken.space.group;
        let span = PreallocSpace {
            group,
            logical_start,
            phys_start: taken.consumed.phys_start,
            phys_len: taken.consumed.phys_len,
        };
        let outcome = inode
            .prealloc_map()
            .update(|preallocs| reserve_in_inode_map(preallocs, span, taken.consumed_end));
        match outcome {
            // Impossible: the span was vacated moments ago inside this same
            // lock section. Leave the counter decremented so the ledger
            // matches the (unchanged) index; the span returns to the pool.
            PreallocInsertion::Rejected => {
                debug_assert!(false, "a taken prealloc span must re-insert cleanly");
                error!(
                    "KExt4 restore of prealloc span (inode {:?}, logical {}, len {}) was \
                     rejected; dropping it",
                    inode.number(),
                    logical_start,
                    taken.consumed.phys_len,
                );
                return;
            }
            PreallocInsertion::Merged {
                pred_key,
                pred_phys_start,
                pred_phys_len,
            } => {
                match alloc.preallocs_by_group.get_mut(&(group, pred_phys_start)) {
                    Some(twin) => {
                        twin.phys_len = BlockCount::new(
                            twin.phys_len
                                .get()
                                .saturating_add(taken.consumed.phys_len.get()),
                        )
                    }
                    None => {
                        // Roll back the inode-side merge that
                        // reserve_in_inode_map already wrote, then give up.
                        inode.prealloc_map().update(|preallocs| {
                            if let Some(merged) = preallocs.remove(&pred_key) {
                                preallocs.insert(
                                    pred_key,
                                    PreallocSpace {
                                        phys_len: pred_phys_len,
                                        ..merged
                                    },
                                );
                            }
                        });
                        debug_assert!(
                            false,
                            "restore predecessor at ({:?}, {:?}) lost its group twin",
                            group, pred_phys_start,
                        );
                        error!(
                            "KExt4 restore predecessor at ({:?}, {:?}) lost its group twin; \
                             dropping the span",
                            group, pred_phys_start,
                        );
                        return;
                    }
                }
            }
            PreallocInsertion::Inserted => {
                alloc.insert_group_prealloc(span, taken.owner.clone());
            }
        }
        alloc.preallocated_blocks += u64::from(taken.consumed.phys_len.get());
        debug_assert_prealloc_conservation!(alloc);
    }

    /// Drops every prealloc space of `inode`, handing its blocks back to the
    /// free pool. In-memory only - these blocks were never marked. Called at the
    /// last writable close and at eviction, so a space never outlives its owner.
    ///
    /// - `inode`: the file whose spaces die; may hold none.
    ///
    /// Returns: blocks handed back to the pool, 0 when the file held none. The
    /// allocator's surplus counter is updated here, so callers may ignore it.
    ///
    /// Locking: takes the allocator mutex, then `inode`'s map leaf lock; caller
    /// must hold neither. Every production caller holds the mount-level ext4
    /// lock (eviction under the read lock, close and writeback under the write
    /// lock), so both acquisitions are uncontended; an inode holding no spaces
    /// costs one mutex round-trip plus an empty-map walk, and the O(spaces)
    /// conservation check is skipped when nothing was released. May sleep.
    pub(crate) fn release_preallocs(&self, inode: &Ext4Inode) -> u64 {
        let mut alloc = lock(&self.allocator);
        let released = inode.prealloc_map().update(|preallocs| {
            let mut released = 0u64;
            for space in preallocs.values() {
                released += u64::from(space.phys_len.get());
                let twin = alloc
                    .preallocs_by_group
                    .remove(&(space.group, space.phys_start));
                check_prealloc_twin(inode.number(), space, twin.as_ref());
            }
            preallocs.clear();
            released
        });
        alloc.preallocated_blocks -= released;
        if released > 0 {
            debug_assert_prealloc_conservation!(alloc);
        }
        released
    }

    /// Cuts a file's prealloc spaces at the new EOF after a shrink truncate:
    /// a space starting at or past `from_logical` dies, the one space
    /// straddling it is shortened to the part below the EOF, and spaces wholly
    /// below stay usable. Freed blocks return to the pool as in
    /// [`Self::release_preallocs`] - no disk work. Linux discards the file's
    /// whole set here; a kext4 space carries an exact logical range with a group
    /// twin, so the prefix below the EOF stays usable.
    ///
    /// - `inode`: the truncated file; may hold no spaces.
    /// - `from_logical`: first block no longer in the file, in blocks; 0 drops
    ///   every space.
    ///
    /// Returns: blocks handed back to the pool - dropped spaces plus a
    /// straddler's trimmed tail - 0 when nothing lay at or past the EOF.
    ///
    /// Locking: takes the allocator mutex, then `inode`'s map leaf lock; caller
    /// must hold neither. May sleep.
    pub(crate) fn clip_preallocs_from_logical(&self, inode: &Ext4Inode, from_logical: u64) -> u64 {
        let mut alloc = lock(&self.allocator);
        let released = inode.prealloc_map().update(|preallocs| {
            let mut released = 0u64;
            // Shorten the one space straddling the new EOF, if any. Its kept
            // prefix is at least one block and shorter than the whole, so
            // the u32 cast and the counter delta are well-defined.
            let straddler = preallocs
                .range(..LogicalBlock::new(from_logical))
                .next_back()
                .map(|(&lstart, space)| (lstart, *space))
                .filter(|(_, space)| space.logical_end() > from_logical);
            if let Some((lstart, space)) = straddler {
                let keep_len = from_logical - lstart.get();
                debug_assert!(keep_len < u64::from(space.phys_len.get()));
                let keep = keep_len as u32;
                released += u64::from(space.phys_len.get() - keep);
                preallocs.insert(
                    lstart,
                    PreallocSpace {
                        phys_len: BlockCount::new(keep),
                        ..space
                    },
                );
                match alloc
                    .preallocs_by_group
                    .get_mut(&(space.group, space.phys_start))
                {
                    Some(twin) => {
                        check_prealloc_twin(inode.number(), &space, Some(twin));
                        twin.phys_len = BlockCount::new(keep);
                    }
                    None => check_prealloc_twin(inode.number(), &space, None),
                }
            }
            // Everything at or past the new EOF dies whole.
            let tail = preallocs.split_off(&LogicalBlock::new(from_logical));
            for space in tail.values() {
                released += u64::from(space.phys_len.get());
                let twin = alloc
                    .preallocs_by_group
                    .remove(&(space.group, space.phys_start));
                check_prealloc_twin(inode.number(), space, twin.as_ref());
            }
            released
        });
        alloc.preallocated_blocks -= released;
        if released > 0 {
            debug_assert_prealloc_conservation!(alloc);
        }
        released
    }

    /// ENOSPC retry valve: hands prealloc spaces back to the pool after an
    /// allocation search came up empty, so the caller can search again. Dooms
    /// whole groups in ascending order until the plan covers `needed`, so the
    /// result can overshoot. Used by the `mballoc` write scan, bounded by
    /// [`PREALLOC_DISCARD_SEARCH_LIMIT`]; the single-group `balloc` entry does
    /// not call it, since a group-unaware discard cannot guarantee reaching
    /// its target group. Nothing is freed on disk - these blocks
    /// were never marked - and a lost space costs only locality. Linux:
    /// `ext4_mb_discard_preallocations`.
    ///
    /// - `needed`: blocks to free, in blocks; 0 sweeps every group holding
    ///   spaces.
    ///
    /// Returns: blocks handed back to the pool. 0 means nothing was discarded,
    /// so a retry cannot do better - that is the caller's stop condition.
    ///
    /// The retry gate is `freed > 0` only; Linux also retries when its
    /// `discard_pa_seq` advances, which the single allocator mutex makes
    /// unnecessary here.
    ///
    /// Locking: takes the allocator mutex, then foreign owners' map leaf locks;
    /// caller must not hold the mutex. May sleep.
    pub(crate) fn discard_preallocs_for_space(&self, needed: u64) -> u64 {
        let mut alloc = lock(&self.allocator);
        // Plan on whole groups, ascending: sum each group's preallocated
        // length, then doom groups until the plan covers `needed`. The index
        // key order already groups entries, so the fold stays ordered.
        let mut preallocated_per_group: BTreeMap<BlockGroupNumber, u64> = BTreeMap::new();
        for (&(group, _), twin) in alloc.preallocs_by_group.iter() {
            *preallocated_per_group.entry(group).or_default() += u64::from(twin.phys_len.get());
        }
        let mut dropped: Vec<BlockGroupNumber> = Vec::new();
        let mut planned = 0u64;
        for (group, preallocated) in &preallocated_per_group {
            if needed != 0 && planned >= needed {
                break;
            }
            dropped.push(*group);
            planned += *preallocated;
        }
        if dropped.is_empty() {
            return 0;
        }
        // Detach the doomed twins before touching any owner: the removals
        // below lock foreign inodes, and no owner lock may be held while
        // this index is iterated.
        let doomed: Vec<GroupPreallocKey> = alloc
            .preallocs_by_group
            .keys()
            .filter(|&&(group, _)| dropped.binary_search(&group).is_ok())
            .copied()
            .collect();
        let mut freed = 0u64;
        for key in doomed {
            // The index cannot lose a key between the enumeration above and
            // here: this whole sweep runs inside one allocator-mutex section.
            let Some(twin) = alloc.preallocs_by_group.remove(&key) else {
                debug_assert!(false, "doomed twin at {:?} vanished mid-sweep", key);
                error!("KExt4 doomed prealloc twin at {:?} vanished mid-sweep", key);
                continue;
            };
            freed += u64::from(twin.phys_len.get());
            let removed = twin
                .owner
                .prealloc_map()
                .update(|preallocs| preallocs.remove(&twin.logical_start));
            match removed {
                Some(space) => {
                    debug_assert_eq!(space.group, key.0);
                    debug_assert_eq!(space.phys_start, key.1);
                    check_prealloc_twin(twin.owner.number(), &space, Some(&twin));
                }
                None => {
                    debug_assert!(
                        false,
                        "discarded group twin at {:?} has no inode-side entry",
                        key,
                    );
                    error!(
                        "KExt4 discarded prealloc twin at {:?} of inode {:?} has no inode-side \
                         entry",
                        key,
                        twin.owner.number(),
                    );
                }
            }
        }
        alloc.preallocated_blocks -= freed;
        debug_assert_prealloc_conservation!(alloc);
        freed
    }
}

unittest::path_mod_test!(
    prealloc_space_unittest,
    "../ut-tests/prealloc_space_unittest.rs"
);
