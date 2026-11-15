// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory mapping backends.
use kalloc::{UsageKind, global_allocator};
use kalloc_types::{AllocFlags, KBox};
use kerrno::{KError, KResult};
use khal::{
    mem::{p2v, v2p},
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use memacct;
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
    if memacct::charge_frame(paddr, pgsize, UsageKind::VirtMem).is_err() {
        global_allocator().dealloc_pages(vaddr.as_usize(), num_pages, UsageKind::VirtMem);
        return Err(KError::NoMemory);
    }

    Ok(paddr)
}

#[doc(hidden)]
pub fn dealloc_frame(frame: PhysAddr, align: PageSize) {
    let vaddr = p2v(frame);
    let page_size: usize = align.into();
    let num_pages = page_size / PAGE_SIZE_4K;
    memacct::uncharge_frame(frame, page_size);
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

/// An owned, one-shot post-populate action.
///
/// Construction is fallible and must precede any state change whose completion
/// requires this action and cannot be rolled back. Dropping an unused hook
/// releases its captures without calling it. Existing in-tree backends return
/// no hook; this type does not add an allocation to those fault paths.
pub struct PopulateHook(KBox<dyn FnMut(&mut MmSpace)>);

impl PopulateHook {
    /// Prepares a hook with the default KERNEL allocation policy.
    ///
    /// Requires sleepable task context with IRQs and preemption enabled, outside
    /// non-sleepable RCU sections and without locks needed by reclaim, IO or
    /// filesystem callbacks. In particular, do not use this while holding an
    /// address-space lock that reclaim may need; prepare before locking instead.
    /// Returns NoMemory if owner allocation fails, dropping the captures.
    pub fn new(callback: impl FnOnce(&mut MmSpace) + 'static) -> KResult<Self> {
        Self::new_with_flags(callback, AllocFlags::KERNEL)
    }

    /// Prepares a hook without sleeping or entering reclaim for owner storage.
    ///
    /// Suitable for a backend that cannot release its MM lock, provided failure
    /// can still be returned or rolled back. Allocator reentrancy and lock-order
    /// constraints still apply; NMI safety, reserves and bounded latency are
    /// not promised. Returns NoMemory and drops captures on allocation failure.
    pub fn new_nowait(callback: impl FnOnce(&mut MmSpace) + 'static) -> KResult<Self> {
        Self::new_with_flags(callback, AllocFlags::NOWAIT)
    }

    /// Prepares a hook with caller-selected owner allocation permissions.
    ///
    /// The context requirements of [`KBox::new_with_flags`] apply. NOIO/NOFS
    /// still permit sleeping and do not exclude all MM reclaim dependencies.
    /// Capture construction/drop has its own context requirements; flags cover
    /// only owner storage. Returns NoMemory or InvalidInput for unknown flags,
    /// releasing captures without invoking the callback in either case.
    pub fn new_with_flags(
        callback: impl FnOnce(&mut MmSpace) + 'static,
        flags: AllocFlags,
    ) -> KResult<Self> {
        let mut callback = Some(callback);
        let owner: KBox<dyn FnMut(&mut MmSpace)> = KBox::new_with_flags(
            move |address_space: &mut MmSpace| {
                if let Some(callback) = callback.take() {
                    callback(address_space);
                }
            },
            flags,
        )?;
        Ok(Self(owner))
    }

    /// Consumes and invokes the hook without allocating owner storage.
    ///
    /// Runs synchronously in the fault caller's context, retaining any outer
    /// MM lock. The callback and capture destructors must respect those locks
    /// and context constraints; this is not deferred work on a separate worker.
    /// It must not reacquire the same MM lock or assume reclaim-safe execution.
    pub fn call(mut self, address_space: &mut MmSpace) {
        (self.0)(address_space);
    }
}

pub(crate) type PopulateResult = KResult<(usize, Option<PopulateHook>)>;

/// Fault-completion payload returned by backends.
pub struct FaultCompletion {
    populated: usize,
    post_action: Option<PopulateHook>,
    cow_conflict_retry: bool,
}

impl FaultCompletion {
    /// Builds a fault-completion payload from a populate result.
    /// Accepts an already prepared hook without allocation or invocation.
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
    /// Taking the hook does not invoke it; dropping it cancels the action.
    pub fn take_post_action(&mut self) -> Option<PopulateHook> {
        self.post_action.take()
    }
}

/// Result type for runtime fault handlers.
pub type FaultCompletionResult = KResult<FaultCompletion>;
