// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Folio allocation, data and dirty/writeback state.

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::{PhysAddr, VirtAddr, v2p};
use log::warn;
use memaddr::PAGE_SIZE_4K;

/// Cached folio stored inside a [`crate::PageCache`].
#[derive(Debug)]
pub struct Folio {
    addr: VirtAddr,
    dirty: bool,
    writeback: bool,
    warn_on_dirty_drop: bool,
}

impl Folio {
    /// Allocates a fresh zero-filled folio.
    pub fn new_zeroed() -> KResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageCache)
            .map_err(|_| KError::NoMemory)?;
        let addr = VirtAddr::from(addr);
        // SAFETY: `alloc_pages` returns a writable virtually mapped page-sized
        // region owned by this folio. Zeroing exactly one page is sound.
        unsafe { core::ptr::write_bytes(addr.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        Ok(Self {
            addr,
            dirty: false,
            writeback: false,
            warn_on_dirty_drop: false,
        })
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

    /// Returns whether a synchronous writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.writeback
    }

    pub(super) fn start_writeback(&mut self) {
        self.writeback = true;
    }

    pub(super) fn end_writeback(&mut self) {
        self.writeback = false;
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub(super) fn set_warn_on_dirty_drop(&mut self, enabled: bool) {
        self.warn_on_dirty_drop = enabled;
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
        if self.dirty && self.warn_on_dirty_drop {
            warn!("dropping dirty file-backed folio without writeback");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
    }
}
