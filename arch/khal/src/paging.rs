// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page table manipulation.

use core::mem::ManuallyDrop;

use kalloc::{PageRef, UsageKind};
use memaddr::{MemoryAddr, PhysAddr, VirtAddr};
use page_table::PagingHandler;
#[doc(no_inline)]
pub use page_table::{
    PageSize, PagingFlags as MappingFlags, PtError as PagingError, PtResult as PagingResult,
};

use crate::mem::p2v;

/// Implementation of [`PagingHandler`], to provide physical memory manipulation
/// to the [page_table] crate.
pub struct PagingHandlerImpl;

impl PagingHandler for PagingHandlerImpl {
    type MappingReference = PageRef;

    fn mapping_reference_paddr(reference: &PageRef) -> PhysAddr {
        reference.phys_addr()
    }

    fn mapping_reference_bytes(reference: &PageRef) -> usize {
        reference.page_count() * memaddr::PAGE_SIZE_4K
    }

    fn mapping_reference_into_raw(reference: PageRef) -> (PhysAddr, usize) {
        let (paddr, pages) = reference.into_raw();
        (paddr, pages * memaddr::PAGE_SIZE_4K)
    }

    unsafe fn mapping_reference_from_raw(paddr: PhysAddr, bytes: usize) -> PageRef {
        // SAFETY: The handler contract supplies the exact outstanding transfer;
        // its byte length was validated against one aligned leaf before admission.
        unsafe { PageRef::from_raw(paddr, bytes / memaddr::PAGE_SIZE_4K) }
    }

    unsafe fn mapping_reference_try_clone_from_raw(
        paddr: PhysAddr,
        bytes: usize,
    ) -> page_table::PtResult<PageRef> {
        debug_assert_ne!(bytes, 0);
        debug_assert_eq!(bytes % memaddr::PAGE_SIZE_4K, 0);
        debug_assert!(paddr.is_aligned(memaddr::PAGE_SIZE_4K));
        // SAFETY: Exclusive table access retains the live leaf's exact transfer
        // for this allocation incarnation. This borrowed view is never dropped
        // or transferred; only try_clone_ref acquires a new independent hold.
        let borrowed =
            ManuallyDrop::new(unsafe { PageRef::from_raw(paddr, bytes / memaddr::PAGE_SIZE_4K) });
        borrowed
            .try_clone_ref()
            .map_err(|_| page_table::PtError::NoMemory)
    }

    fn alloc_frame() -> Option<PhysAddr> {
        assert!(
            kalloc::is_page_allocator_ready(),
            "page-table allocation requested before page allocator initialization"
        );
        kalloc::alloc_page_ref(UsageKind::PageTable)
            .map(|page| {
                let (paddr, num_pages) = page.into_raw();
                debug_assert_eq!(num_pages, 1);
                paddr
            })
            .ok()
    }

    fn dealloc_frame(paddr: PhysAddr) {
        // SAFETY: page_table consumes exactly one frame allocated by this
        // handler with range length one, after its owner ended hardware use and translation
        // retirement. Borrowed root entries do not release their source frames.
        drop(unsafe { PageRef::from_raw(paddr, 1) });
    }

    #[inline]
    fn p2v(paddr: PhysAddr) -> VirtAddr {
        p2v(paddr)
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::x86_64::X64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::x86_64::X64PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::riscv::Sv39PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::riscv::Sv39PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(target_arch = "aarch64")]{
        /// The architecture-specific page table.
        pub type PageTable = page_table::aarch64::A64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::aarch64::A64PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(target_arch = "loongarch64")] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::loongarch64::LA64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::loongarch64::LA64PageTableMut<'a, PagingHandlerImpl>;
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(pub mod tests_paging, "../ut-tests/paging.rs");

#[cfg(unittest)]
unittest::path_mod_test!(tests_page_refs, "../ut-tests/paging_page_refs.rs");
