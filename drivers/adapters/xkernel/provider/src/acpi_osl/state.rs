// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared mutable state behind the cloneable ACPI host handle.

use core::sync::atomic::{AtomicBool, AtomicUsize};

use khal::firmware::HostFailure;
use kspin::SpinNoIrq;

/// Maximum number of distinct table ranges retained for read-only enforcement.
/// Each table is mapped at least as a header and as a full table, and the
/// vendor re-enumerates tables on every lookup, so the table must tolerate
/// repeated maps of the same range without growing.
pub(super) const MAX_TABLE_RANGES: usize = 256;
/// Maximum number of live writable mappings. The vendor holds at most one
/// mapping per mapped GAS or wake mailbox; the table leaves room for later
/// additions without unbounded state.
pub(super) const MAX_WRITABLE_RANGES: usize = 32;
/// Maximum number of AML mutexes the adapter can hand out.
pub(super) const MAX_MUTEXES: usize = 32;

/// A fixed-capacity set of half-open physical ranges.
#[derive(Clone, Copy)]
pub(super) struct RangeSet<const N: usize> {
    ranges: [(usize, usize); N],
    len: usize,
}

impl<const N: usize> RangeSet<N> {
    pub(super) const fn new() -> Self {
        Self {
            ranges: [(0, 0); N],
            len: 0,
        }
    }

    /// Retains one range. Returns `false` when the bound is reached.
    ///
    /// The vendor maps the same table header and full table repeatedly while
    /// enumerating, so an already-retained range is accepted without consuming
    /// another slot.
    pub(super) fn push(&mut self, start: usize, end: usize) -> bool {
        if self.ranges[..self.len].contains(&(start, end)) {
            return true;
        }
        if self.len == N {
            return false;
        }
        self.ranges[self.len] = (start, end);
        self.len += 1;
        true
    }

    /// Removes one exact range, if retained.
    pub(super) fn remove(&mut self, start: usize, end: usize) {
        if let Some(index) = self.ranges[..self.len]
            .iter()
            .position(|&range| range == (start, end))
        {
            self.len -= 1;
            self.ranges[index] = self.ranges[self.len];
        }
    }

    /// Returns whether `[start, end)` touches a retained range.
    pub(super) fn overlaps(&self, start: usize, end: usize) -> bool {
        self.ranges[..self.len]
            .iter()
            .any(|&(range_start, range_end)| start < range_end && end > range_start)
    }
}

/// Physical ranges mapped for ACPI table loading. Retained after the read-only
/// mapping is dropped so later SystemMemory writes can be rejected.
pub(super) type TableRanges = RangeSet<MAX_TABLE_RANGES>;
/// Object ranges (`T`) of live writable mappings. Removing the range on unmap
/// is what makes the host's exclusivity guarantee re-usable.
pub(super) type WritableRanges = RangeSet<MAX_WRITABLE_RANGES>;

/// State shared by every clone of the host adapter.
pub(super) struct HostState {
    /// First failure recorded; later failures do not overwrite it.
    pub(super) failure: SpinNoIrq<Option<HostFailure>>,
    /// Sticky gate: once set, no later host access touches hardware.
    pub(super) failed: AtomicBool,
    /// Table ranges that must not be written through SystemMemory.
    pub(super) table_ranges: SpinNoIrq<TableRanges>,
    /// Object ranges of live writable mappings. A mapping is held by the
    /// vendor for as long as its `PhysicalMapping` is alive; the host rejects
    /// a second writable mapping whose `T` overlaps one of these.
    pub(super) writable_ranges: SpinNoIrq<WritableRanges>,
    /// Next AML mutex handle.
    pub(super) next_mutex: AtomicUsize,
    /// Whether each handed-out mutex is held.
    pub(super) mutexes: [AtomicBool; MAX_MUTEXES],
}

impl HostState {
    pub(super) fn new() -> Self {
        Self {
            failure: SpinNoIrq::new(None),
            failed: AtomicBool::new(false),
            table_ranges: SpinNoIrq::new(TableRanges::new()),
            writable_ranges: SpinNoIrq::new(WritableRanges::new()),
            next_mutex: AtomicUsize::new(0),
            mutexes: core::array::from_fn(|_| AtomicBool::new(false)),
        }
    }
}
