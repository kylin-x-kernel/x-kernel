// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Folio allocation, data and dirty/writeback state.

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use log::warn;
use memaddr::PAGE_SIZE_4K;

use crate::PageIndex;

/// Cached folio stored inside a [`crate::MappingPages`].
#[derive(Debug)]
pub struct Folio {
    index: PageIndex,
    addr: VirtAddr,
    uptodate: bool,
    dirty: bool,
    is_invalidated: bool,
    writeback: bool,
}

impl Folio {
    /// Allocates a non-uptodate, zero-filled folio with a stable index.
    pub fn new_zeroed(index: PageIndex) -> KResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageCache)
            .map_err(|_| KError::NoMemory)?;
        let addr = VirtAddr::from(addr);
        // SAFETY: `alloc_pages` returns a writable virtually mapped page-sized
        // region owned by this folio. Zeroing exactly one page is sound.
        unsafe { core::ptr::write_bytes(addr.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        Ok(Self {
            index,
            addr,
            uptodate: false,
            dirty: false,
            is_invalidated: false,
            writeback: false,
        })
    }

    /// Returns whether backing data has been initialized successfully.
    pub fn is_uptodate(&self) -> bool {
        self.uptodate
    }

    /// Makes a completely initialized folio available to cache readers.
    pub fn mark_uptodate(&mut self) {
        self.uptodate = true;
    }

    /// Returns the immutable page index; this does not prove mapping residency.
    pub const fn index(&self) -> PageIndex {
        self.index
    }

    /// Marks a removed cache entry unusable by previously collected readers.
    pub(crate) fn invalidate(&mut self) {
        self.is_invalidated = true;
        self.clear_dirty();
    }

    pub(crate) fn is_invalidated(&self) -> bool {
        self.is_invalidated
    }

    /// Returns the physical address of this folio.
    pub fn paddr(&self) -> PhysAddr {
        v2p(self.addr)
    }

    /// Marks the folio dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns whether the folio is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns whether a writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.writeback
    }

    pub(super) fn start_writeback(&mut self) {
        debug_assert!(!self.writeback);
        self.writeback = true;
    }

    pub(super) fn end_writeback(&mut self) {
        debug_assert!(self.writeback);
        self.writeback = false;
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns the folio contents as a mutable 4 KiB slice.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the folio uniquely owns the page at `self.addr`; callers
        // obtain `&mut self`, so producing a mutable slice for that page is
        // exclusive for the duration of the borrow.
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE_4K) }
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        if self.dirty || self.writeback {
            warn!(
                "dropping folio {} with dirty={} writeback={}",
                self.index, self.dirty, self.writeback
            );
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}
