// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory mapping backends.
use alloc::boxed::Box;

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::{
    mem::{p2v, v2p},
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use memaddr::{DynPageIter, MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
pub mod linear;
pub mod private;
pub mod shared;

pub use private::{PrivateBackend, private_anon_invalidate_notifier};
pub use shared::SharedPages;

use crate::{FaultContext, MmSpace, vma::VmBackingInfo};

#[doc(hidden)]
pub fn divide_page(size: usize, pgsize: PageSize) -> usize {
    assert!(pgsize.is_aligned(size), "unaligned");
    size >> (pgsize as usize).trailing_zeros()
}

#[doc(hidden)]
pub fn alloc_frame(zeroed: bool, size: PageSize) -> KResult<PhysAddr> {
    let pgsize = size as usize;
    let num_pages = pgsize / PAGE_SIZE_4K;
    let vaddr = VirtAddr::from(
        global_allocator()
            .alloc_pages(num_pages, pgsize, UsageKind::VirtMem)
            .map_err(|_| KError::NoMemory)?,
    );
    if zeroed {
        // SAFETY: `vaddr` names a freshly allocated virtual region of `pgsize`
        // bytes, so zero-filling that range is valid.
        unsafe { core::ptr::write_bytes(vaddr.as_mut_ptr(), 0, pgsize) };
    }
    let paddr = v2p(vaddr);

    Ok(paddr)
}

#[doc(hidden)]
pub fn dealloc_frame(frame: PhysAddr, align: PageSize) {
    let vaddr = p2v(frame);
    let page_size: usize = align.into();
    let num_pages = page_size / PAGE_SIZE_4K;
    global_allocator().dealloc_pages(vaddr.as_usize(), num_pages, UsageKind::VirtMem);
}

#[doc(hidden)]
pub fn pages_in(range: VirtAddrRange, align: PageSize) -> KResult<DynPageIter<VirtAddr>> {
    DynPageIter::new(range.start, range.end, align as usize).ok_or(KError::InvalidInput)
}

#[doc(hidden)]
pub fn map_paging_err(err: PagingError) -> KError {
    match err {
        PagingError::NoMemory => KError::NoMemory,
        _ => KError::InvalidInput,
    }
}

pub trait BackendOps {
    /// Returns the page size of the backend.
    fn page_size(&self) -> PageSize;

    /// Returns a Linux-aligned description of the VMA backing object.
    fn backing_info(&self) -> VmBackingInfo;

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult;

    /// Called before a memory region is protected.
    fn on_protect(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        Ok(new_flags)
    }

    /// Populate a memory region and return how many pages now satisfy
    /// `access_flags`.
    ///
    /// If another thread has already mapped the page with sufficient permissions,
    /// treat it as populated.
    fn populate(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _access_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> PopulateResult {
        Ok((0, None))
    }

    /// Materialize the page(s) needed to satisfy a fault.
    ///
    /// The default path reuses `populate()`, while backend-specific fault
    /// handlers can override this method.
    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let range = VirtAddrRange::from_start_size(
            ctx.address().align_down(self.page_size()),
            self.page_size() as usize,
        );
        self.populate(range, flags, ctx.access_flags(), pgtbl)
            .map(FaultCompletion::from_populate)
    }
}

type PopulateHook = Box<dyn FnOnce(&mut MmSpace)>;
pub(crate) type PopulateResult = KResult<(usize, Option<PopulateHook>)>;

/// Fault-completion payload returned by backends.
pub struct FaultCompletion {
    populated: usize,
    post_action: Option<PopulateHook>,
    cow_conflict_retry: bool,
}

impl FaultCompletion {
    /// Builds a fault-completion payload from a populate result.
    pub fn from_populate((populated, post_action): (usize, Option<PopulateHook>)) -> Self {
        Self {
            populated,
            post_action,
            cow_conflict_retry: false,
        }
    }

    /// Builds a retry payload for a COW compare/replace conflict.
    pub const fn cow_conflict_retry() -> Self {
        Self {
            populated: 0,
            post_action: None,
            cow_conflict_retry: true,
        }
    }

    /// Returns the number of pages materialized for the fault.
    pub const fn populated(&self) -> usize {
        self.populated
    }

    /// Returns `true` when backend state changed during COW resolution and
    /// the fault should be retried instead of reported as no progress.
    pub const fn is_cow_conflict_retry(&self) -> bool {
        self.cow_conflict_retry
    }

    /// Takes the deferred post-fault action, if the backend produced one.
    pub fn take_post_action(&mut self) -> Option<PopulateHook> {
        self.post_action.take()
    }
}

/// Result type for runtime fault handlers.
pub type FaultCompletionResult = KResult<FaultCompletion>;
