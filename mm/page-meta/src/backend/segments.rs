// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use memaddr::{PAGE_SIZE_4K, PhysAddrRange};

use crate::{PageMeta, PageMetaError, free_ranges};

const CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct Segment {
    start_pfn: usize,
    end_pfn: usize,
    first_index: usize,
}

impl Segment {
    const EMPTY: Self = Self {
        start_pfn: 0,
        end_pfn: 0,
        first_index: 0,
    };
}

// The common boot path supplies sorted, disjoint, page-aligned ranges.
// Capture their complete coverage before storage is removed from the inventory.
fn build_segments(
    ranges: &[PhysAddrRange],
    segments: &mut [Segment; CAPACITY],
) -> Result<(usize, usize), PageMetaError> {
    let mut segment_count = 0;
    let mut entry_count = 0usize;
    for range in ranges.iter().filter(|r| !r.is_empty()) {
        if segment_count == CAPACITY {
            return Err(PageMetaError::CapacityExceeded);
        }
        let start_pfn = range.start.as_usize() / PAGE_SIZE_4K;
        let end_pfn = range.end.as_usize() / PAGE_SIZE_4K;
        segments[segment_count] = Segment {
            start_pfn,
            end_pfn,
            first_index: entry_count,
        };
        entry_count = entry_count
            .checked_add(end_pfn - start_pfn)
            .ok_or(PageMetaError::InvalidRange)?;
        segment_count += 1;
    }
    Ok((segment_count, entry_count))
}

fn storage_bytes(entry_count: usize) -> Result<usize, PageMetaError> {
    entry_count
        .checked_mul(core::mem::size_of::<PageMeta>())
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(PageMetaError::InvalidRange)
}

/// Builds the private index and its storage before any table is published.
///
/// # Safety
/// Ranges must be normalized by free_ranges::normalize. The caller exclusively owns
/// their RAM, and map returns exclusive writable backing aligned for PageMeta
/// and valid until the table and all descriptor borrows end.
pub(crate) unsafe fn init(
    ranges: &mut [PhysAddrRange],
    map: &mut impl FnMut(PhysAddrRange) -> Result<NonNull<u8>, PageMetaError>,
) -> Result<Table, PageMetaError> {
    let mut segments = [Segment::EMPTY; CAPACITY];
    let (segment_count, entry_count) = build_segments(ranges, &mut segments)?;
    let bytes = storage_bytes(entry_count)?;
    free_ranges::with_reserved_storage(ranges, bytes, |backing| {
        let pointer = map(backing)?.cast();
        // SAFETY: with_reserved_storage selected enough exclusive boot RAM.
        // The caller's map contract supplies a full writable allocation aligned
        // for PageMeta and retained for the table's lifetime. build_segments
        // produced matching index/counts; initialize checks the descriptor-array
        // size and available byte capacity.
        let mut table = unsafe {
            Table::initialize(
                segments,
                segment_count,
                entry_count,
                pointer,
                backing.size(),
            )
        }?;
        table.mark_permanent_range(
            backing.start.as_usize() / PAGE_SIZE_4K,
            backing.size() / PAGE_SIZE_4K,
        )?;
        Ok(table)
    })
}

/// Metadata table over physical-page identities in caller-owned storage.
///
/// An exclusive table may mark reservations before publication. Shared
/// lookups never mutate descriptors. Dropping the table does not reclaim
/// its backing or the physical pages it describes; the caller owns that storage.
pub(crate) struct Table {
    segments: [Segment; CAPACITY],
    segment_count: usize,
    descriptors: NonNull<PageMeta>,
}

// SAFETY: this table uniquely owns its metadata access while being prepared.
// The supplied storage remains valid for its lifetime, and moving it transfers
// that access without moving the descriptor backing.
unsafe impl Send for Table {}
// SAFETY: shared lookups expose immutable initialized descriptors. Reservation
// requires &mut self before publication; shared borrows exclude those writes.
unsafe impl Sync for Table {}

impl Table {
    pub(crate) fn covered_pages(&self) -> usize {
        self.segments[..self.segment_count]
            .last()
            .map_or(0, |segment| {
                segment.first_index + (segment.end_pfn - segment.start_pfn)
            })
    }

    /// Initializes descriptors in storage supplied by the boot owner.
    ///
    /// # Errors
    ///
    /// Rejects insufficient storage or descriptor-array overflow. Failure does
    /// not write storage.
    ///
    /// # Safety
    ///
    /// The pointer must identify exclusive writable storage of backing_bytes
    /// bytes within one allocation, aligned for PageMeta and valid until the
    /// table and all descriptor borrows are gone. The caller must exclude that
    /// storage from allocation and prevent all other access while this table owns
    /// it. The segment table and both counts must be matching outputs of build_segments over
    /// normalized ranges. Covered page contents need no kernel mapping.
    unsafe fn initialize(
        segments: [Segment; CAPACITY],
        segment_count: usize,
        entry_count: usize,
        descriptors: NonNull<PageMeta>,
        backing_bytes: usize,
    ) -> Result<Self, PageMetaError> {
        let bytes = storage_bytes(entry_count)?;
        if bytes > backing_bytes {
            return Err(PageMetaError::InsufficientStorage);
        }
        for index in 0..entry_count {
            // SAFETY: storage_bytes and backing_bytes bound this index within
            // the single allocation supplied under the caller's # Safety
            // contract. No descriptor references have been published.
            let slot = unsafe { descriptors.as_ptr().add(index) };
            // SAFETY: the caller's # Safety contract provides exclusive writable
            // storage aligned for PageMeta. This in-bounds element offset
            // preserves that alignment, and write initializes without reading.
            unsafe { slot.write(PageMeta::new()) };
        }
        Ok(Self {
            segments,
            segment_count,
            descriptors,
        })
    }

    fn segment_for_pfn(&self, pfn: usize) -> Option<&Segment> {
        let segments = &self.segments[..self.segment_count];
        let upper = segments.partition_point(|segment| segment.start_pfn <= pfn);
        let segment = segments.get(upper.checked_sub(1)?)?;
        (pfn < segment.end_pfn).then_some(segment)
    }

    /// Borrows the descriptor for a covered PFN, or returns None for a hole.
    /// The borrow grants no ownership, page-content access, or release right.
    pub(crate) fn lookup(&self, pfn: usize) -> Option<&PageMeta> {
        let segment = self.segment_for_pfn(pfn)?;
        let index = segment.first_index + (pfn - segment.start_pfn);
        // SAFETY: the physical table selects an initialized slot within
        // supplied storage, whose lifetime covers this table borrow.
        let slot = unsafe { self.descriptors.as_ptr().add(index) };
        // SAFETY: initialization constructed this selected slot and the backing
        // outlives the table borrow. The shared borrow excludes reservation.
        Some(unsafe { &*slot })
    }

    /// Marks covered pages as permanently reserved using exclusive access.
    ///
    /// Reservations must be installed before publishing shared lookups. Empty
    /// ranges are accepted; marking an already reserved page is idempotent.
    ///
    /// # Errors
    ///
    /// Returns InvalidRange for overflow or NotCovered for a range crossing a
    /// coverage boundary. The entire range is checked before any flag changes.
    fn mark_permanent_range(
        &mut self,
        start_pfn: usize,
        pages: usize,
    ) -> Result<(), PageMetaError> {
        if pages == 0 {
            return Ok(());
        }
        let segment = self
            .segment_for_pfn(start_pfn)
            .ok_or(PageMetaError::NotCovered)?;
        let end = start_pfn
            .checked_add(pages)
            .ok_or(PageMetaError::InvalidRange)?;
        if end > segment.end_pfn {
            return Err(PageMetaError::NotCovered);
        }
        let first = segment.first_index + start_pfn - segment.start_pfn;
        for index in first..first + pages {
            // SAFETY: the checked segment selects initialized metadata storage.
            // Exclusive table access excludes all lookup borrows, and the
            // caller retains this backing throughout boot preparation.
            let slot = unsafe { self.descriptors.as_ptr().add(index) };
            // SAFETY: this initialized slot is exclusively accessible through
            // &mut self; no descriptor borrow exists while its flag is changed.
            unsafe { (*slot).mark_permanent() };
        }
        Ok(())
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/segments.rs");
