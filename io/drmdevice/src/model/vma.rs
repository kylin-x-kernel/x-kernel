// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The card's mmap offset namespace (`drm_vma_manager.c`).
//!
//! `MAP_DUMB` and `mmap` do not name a buffer by its handle: they name it by
//! the *virtual offset* the driver gave it, and the mapping offset is a byte
//! position inside that namespace. Linux keeps the mapping in a
//! `drm_vma_offset_manager`, one per device, and this is the same idea in the
//! same place — the place where a `drm_gem_object` gets its `vma_node`.
//!
//! Two facts are separated, exactly as in Linux:
//!
//! * the **offset** is the identity a client passes to `mmap`, allocated once
//!   per object and stable for its life;
//! * the **range** is the byte extent the object covers, so an offset can be
//!   checked against the card's namespace and overlapping ranges rejected.
//!
//! Strides are page-aligned like `drm_vma_offset_add`, so every offset is a
//! valid `mmap` offset and two objects can never share a page.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

use super::error::DrmResult;
use crate::prelude::*;

/// The mmap offset manager of one card.
///
/// Offsets are handed out from a monotonic counter and never reused while the
/// card lives, which is what makes a stale offset fail to map instead of
/// quietly landing in another buffer.
#[derive(Default)]
pub(crate) struct VmaOffsetManager {
    /// Offset of every live allocation, keyed by its start.
    allocations: Mutex<BTreeMap<u64, VmaRange>>,
    /// The next free offset.
    next: AtomicU64,
}

/// One allocation of the namespace.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VmaRange {
    /// Byte offset the client passes to `mmap`.
    pub(crate) offset: u64,
    /// Byte length the object covers, page rounded.
    pub(crate) size: u64,
}

impl VmaOffsetManager {
    /// Reserves `size` bytes and returns the offset naming them.
    ///
    /// `size` is rounded up to whole pages: an `mmap` of a partial page still
    /// maps one page, and two allocations must not share it.
    pub(crate) fn allocate(&self, size: u64) -> DrmResult<u64> {
        let size = size.max(1).next_multiple_of(PAGE_SIZE_4K as u64);
        let mut allocations = self.allocations.lock();
        let offset = self.next.load(Ordering::Relaxed);
        let end = offset.checked_add(size).ok_or(DrmError::NoMemory)?;
        self.next.store(end, Ordering::Relaxed);
        allocations.insert(offset, VmaRange { offset, size });
        Ok(offset)
    }

    /// The allocation a client's `mmap` offset names.
    ///
    /// The mapping may start anywhere inside an allocation (a client mapping
    /// the tail of a buffer), so the lookup is by containment rather than by
    /// equality. An unknown offset is `EINVAL`, as `drm_vma_offset_lookup_locked`
    /// failing is.
    pub(crate) fn lookup(&self, offset: u64) -> Option<VmaRange> {
        let allocations = self.allocations.lock();
        allocations
            .range(..=offset)
            .next_back()
            .map(|(_, range)| *range)
            .filter(|range| offset < range.offset + range.size)
    }

    /// The allocation that covers the whole page range `[offset, offset + pages)`.
    ///
    /// This is `drm_vma_offset_exact_lookup_locked`: `drm_gem_mmap` accepts a
    /// mapping only when the object contains all of it, so a client cannot map
    /// past the end of a buffer into whatever follows it in the namespace.
    pub(crate) fn lookup_exact(&self, offset: u64, bytes: u64) -> Option<VmaRange> {
        let range = self.lookup(offset)?;
        (offset
            .checked_add(bytes)
            .is_some_and(|end| end <= range.offset + range.size))
        .then_some(range)
    }

    /// Releases an allocation.
    ///
    /// The offset is not reused even after this: a client that kept the number
    /// from a destroyed handle must never be handed a different buffer by a
    /// later `mmap`, so the counter only moves forward.
    pub(crate) fn release(&self, offset: u64) {
        let _ = self.allocations.lock().remove(&offset);
    }
}
