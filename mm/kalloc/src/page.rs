// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global page owners and allocation helpers.
use core::{fmt, mem::ManuallyDrop};

use alloc_engine::{AllocError, AllocResult};
use kerrno::KResult;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use page_meta::{PageMeta, PageMetaError, ReferenceState};

use crate::{UsageKind, global_allocator};

/// A RAII wrapper for contiguous 4K-sized pages.
///
/// Automatically deallocates the pages when dropped.
#[derive(Debug)]
pub struct GlobalPage {
    start_va: VirtAddr,
    num_pages: usize,
}

impl GlobalPage {
    /// Reconstruct a `GlobalPage` from a known VA and page count.
    ///
    /// # Safety
    ///
    /// `start_va` must have been returned by a prior `alloc_pages` with the
    /// same `num_pages`, and no other `GlobalPage` currently owns it.
    pub unsafe fn from_raw(start_va: VirtAddr, num_pages: usize) -> Self {
        Self {
            start_va,
            num_pages,
        }
    }

    /// Allocates one 4K-sized page.
    pub fn alloc() -> KResult<Self> {
        Self::alloc_pages(1)
    }

    /// Allocates one 4K-sized page and initializes it with zero.
    pub fn alloc_zero() -> KResult<Self> {
        let mut page = Self::alloc()?;
        page.zero();
        Ok(page)
    }

    /// Allocates contiguous 4K-sized pages.
    pub fn alloc_contiguous(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        Self::alloc_pages_with_alignment(num_pages, align_pow2)
    }

    /// Get the start virtual address of the page.
    pub fn start_va(&self) -> VirtAddr {
        self.start_va
    }

    /// Get the start physical address by converting the virtual address.
    pub fn start_pa<F>(&self, v2p: F) -> PhysAddr
    where
        F: FnOnce(VirtAddr) -> PhysAddr,
    {
        v2p(self.start_va)
    }

    /// Get the total size (in bytes) of these pages.
    pub fn size(&self) -> usize {
        self.num_pages * PAGE_SIZE_4K
    }

    /// Convert the start virtual address to a raw pointer.
    pub fn as_ptr(&self) -> *const u8 {
        self.start_va.as_ptr()
    }

    /// Convert the start virtual address to a mutable raw pointer.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.start_va.as_mut_ptr()
    }

    /// Fills the allocated pages with a specific byte value.
    pub fn fill(&mut self, byte: u8) {
        // SAFETY: `self` owns a contiguous allocation of `self.size()` bytes
        // starting at `self.as_mut_ptr()`, so writing that full range is valid.
        unsafe { core::ptr::write_bytes(self.as_mut_ptr(), byte, self.size()) }
    }

    /// Fills the allocated pages with zero.
    pub fn zero(&mut self) {
        self.fill(0)
    }

    // Note: The following conversions from raw pointers and sizes to slices
    // utilize standard Rust `core::slice` idioms. These fundamental memory
    // operations are universally identical across Rust projects and should
    // not be interpreted as literal duplication.
    /// Returns a slice for reading data.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `self` owns a live allocation of `self.size()` bytes
        // starting at `self.as_ptr()`, and this method returns a shared view.
        unsafe { core::slice::from_raw_parts(self.as_ptr(), self.size()) }
    }

    /// Returns a mutable slice for writing data.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: `&mut self` guarantees exclusive access to the live
        // allocation of `self.size()` bytes starting at `self.as_mut_ptr()`.
        unsafe { core::slice::from_raw_parts_mut(self.as_mut_ptr(), self.size()) }
    }

    /// Internal function to allocate pages.
    fn alloc_pages(num_pages: usize) -> KResult<Self> {
        let va = global_allocator()
            .alloc_pages(num_pages, PAGE_SIZE_4K, UsageKind::Global)
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => kerrno::KError::NoMemory,
                _ => kerrno::KError::InvalidInput,
            })?;
        Ok(Self {
            start_va: va.into(),
            num_pages,
        })
    }

    /// Internal function to allocate pages with specific alignment.
    fn alloc_pages_with_alignment(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        let va = global_allocator()
            .alloc_pages(num_pages, align_pow2, UsageKind::Global)
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => kerrno::KError::NoMemory,
                _ => kerrno::KError::InvalidInput,
            })?;
        Ok(Self {
            start_va: va.into(),
            num_pages,
        })
    }
}

impl Drop for GlobalPage {
    fn drop(&mut self) {
        global_allocator().dealloc_pages(self.start_va.into(), self.num_pages, UsageKind::Global);
    }
}

/// One owned lifetime hold on a physical base page in the global metadata table.
///
/// Clone adds a hold to the existing metadata counter; final Drop returns the
/// page to kalloc's global inventory. This grants no exclusive content access,
/// kernel mapping, or authority to modify a PTE. Holders retain ownership through
/// any required translation/device retirement. Final release may run in ordinary or
/// IRQ context, never NMI, and does not sleep. Ordinary descriptor borrows cannot
/// create this value. Moving it transfers one existing hold without counting.
pub struct PageRef {
    meta: &'static PageMeta,
}

const _: () = assert!(core::mem::size_of::<PageRef>() == core::mem::size_of::<&PageMeta>());

impl PageRef {
    /// Starts one hold for a newly allocated exclusive, unpublished base page.
    ///
    /// # Errors
    /// Returns InvalidRange for an unaligned address, or NotCovered before global
    /// metadata publication and outside its physical coverage.
    ///
    /// # Panics
    /// Panics if the allocator supplied a referenced or permanently reserved page.
    ///
    /// # Safety
    /// paddr must be an exclusive new one-page allocation from kalloc's global
    /// inventory. No raw user or prior hold may access or release it. kind must
    /// match its initial accounting class. On success ownership transfers here;
    /// on error the caller retains the unclaimed page.
    ///
    /// kind is retained for final accounting and does not establish page type,
    /// current users, content-access rights or reclaimability.
    unsafe fn from_allocated(paddr: PhysAddr, kind: UsageKind) -> Result<Self, PageMetaError> {
        if !paddr.as_usize().is_multiple_of(PAGE_SIZE_4K) {
            return Err(PageMetaError::InvalidRange);
        }
        let meta =
            page_meta::lookup(paddr.as_usize() / PAGE_SIZE_4K).ok_or(PageMetaError::NotCovered)?;
        // SAFETY: the caller supplies this page's exclusive unpublished inventory
        // allocation; the global lookup selects its exact resident descriptor.
        let claimed = unsafe { meta.claim_reference(kind) };
        assert_eq!(
            claimed,
            Ok(()),
            "allocator returned a referenced/reserved page"
        );
        Ok(Self { meta })
    }

    /// Returns the physical base-page number of this live allocation.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: constructors select a descriptor from the \
                  immutable global table"
    )]
    pub fn pfn(&self) -> usize {
        page_meta::pfn_of(self.meta).expect("live page descriptor must have a global PFN")
    }

    /// Returns the physical base address without granting content access.
    pub fn phys_addr(&self) -> PhysAddr {
        (self.pfn() * PAGE_SIZE_4K).into()
    }

    /// Adds a separately owned hold without allocating another control block.
    ///
    /// # Errors
    /// Returns ReferenceOverflow if another hold would exceed the count limit.
    pub fn try_clone_ref(&self) -> Result<Self, PageMetaError> {
        // SAFETY: self keeps an existing hold live throughout the increment;
        // the new PageRef consumes the additional hold exactly once on Drop.
        let result = unsafe { self.meta.clone_reference() };
        assert_ne!(
            result,
            Err(PageMetaError::InvalidState),
            "live hold lost its reference state"
        );
        result?;
        Ok(Self { meta: self.meta })
    }

    /// Transfers this hold to a raw PA owner without changing the count.
    /// The recipient must eventually consume the transfer once with from_raw.
    pub fn into_raw(self) -> PhysAddr {
        let owner = ManuallyDrop::new(self);
        owner.phys_addr()
    }

    /// Consumes one outstanding raw transfer without incrementing the counter.
    /// The original accounting class stays in metadata and is not supplied again.
    ///
    /// # Safety
    /// paddr must identify exactly one unconsumed into_raw transfer from this
    /// allocation incarnation. Retain the restored hold through all required
    /// translation/device completion before releasing it. A lookup or bare
    /// raw allocator result does not establish this transfer ownership.
    ///
    /// # Panics
    /// Panics for an unaligned, uncovered or non-referenced address.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the exact raw-transfer contract requires \
                  a live covered descriptor"
    )]
    pub unsafe fn from_raw(paddr: PhysAddr) -> Self {
        assert!(paddr.as_usize().is_multiple_of(PAGE_SIZE_4K));
        let meta = page_meta::lookup(paddr.as_usize() / PAGE_SIZE_4K)
            .expect("raw page transfer requires global metadata");
        assert!(matches!(
            meta.reference_state(),
            ReferenceState::Referenced { .. }
        ));
        Self { meta }
    }
}

impl fmt::Debug for PageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageRef").field("pfn", &self.pfn()).finish()
    }
}

impl Drop for PageRef {
    fn drop(&mut self) {
        // SAFETY: self owns one outstanding hold, consumed once by this Drop;
        // its users must end content/hardware use before ending that hold.
        if unsafe { self.meta.release_reference() } {
            let paddr = self.phys_addr();
            // SAFETY: the true result selected this unique retiring holder.
            // Storage remains outside inventory until the direct release below.
            let kind = unsafe { self.meta.finish_retirement() };
            global_allocator().dealloc_pages(kaddr_layout::p2v(paddr.as_usize()), 1, kind);
        }
    }
}

/// Allocates one uninitialized, reference-owned base page from global inventory.
///
/// The category is charged once until the last hold ends. Clone does not add a
/// control block or repeat the charge. No private allocator inventory is added.
///
/// # Errors
/// Returns NotInitialized before allocator or metadata setup, or for a page with
/// no metadata; NoMemory on inventory exhaustion; and InvalidInput for an invalid
/// page or an allocation outside the kernel direct mapping.
pub fn alloc_page_ref(kind: UsageKind) -> AllocResult<PageRef> {
    let allocator = global_allocator();
    if !allocator.is_page_allocator_ready() {
        return Err(AllocError::NotInitialized);
    }
    let va = allocator.alloc_pages(1, PAGE_SIZE_4K, kind)?;
    let paddr = PhysAddr::from(kaddr_layout::v2p(va));
    let result = if kaddr_layout::p2v(paddr.as_usize()) != va {
        Err(PageMetaError::InvalidRange)
    } else {
        // SAFETY: alloc_pages supplied one exclusive unpublished page from the
        // global inventory used by PageRef::drop. No other holder exists.
        unsafe { PageRef::from_allocated(paddr, kind) }
    };
    match result {
        Ok(frame) => {
            if kind == UsageKind::RustHeap {
                // Raw heap backing omits this charge; an owned whole page does not.
                allocator.record_usage_alloc(kind, PAGE_SIZE_4K);
            }
            Ok(frame)
        }
        Err(error) => {
            if kind != UsageKind::RustHeap {
                allocator.record_usage_dealloc(kind, PAGE_SIZE_4K);
            }
            allocator.release_pages_to_inventory(va, 1);
            Err(match error {
                PageMetaError::NotCovered => AllocError::NotInitialized,
                _ => AllocError::InvalidInput,
            })
        }
    }
}

/// Allocates a zero-filled page with one lifetime hold from global inventory.
///
/// This uses the same allocation and lifetime protocol as alloc_page_ref.
/// Zeroing occurs while the new page is still exclusive and unpublished.
///
/// # Errors
/// Returns the initialization, inventory or mapping errors of alloc_page_ref.
pub fn alloc_zeroed_page_ref(kind: UsageKind) -> AllocResult<PageRef> {
    let page = alloc_page_ref(kind)?;
    let address = kaddr_layout::p2v(page.phys_addr().as_usize());
    // SAFETY: this newly allocated page has one unpublished hold and a full-page
    // writable direct mapping with no content aliases. No clone has escaped.
    unsafe { core::ptr::write_bytes(address as *mut u8, 0, PAGE_SIZE_4K) };
    Ok(page)
}

unittest::path_mod_test!(tests, "../ut-tests/page_reference.rs");
