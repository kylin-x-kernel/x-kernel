//! Memory mapping backends.
use alloc::{boxed::Box, sync::Arc};

use axerrno::{AxError, AxResult};
use enum_dispatch::enum_dispatch;
use kalloc::{UsageKind, global_allocator};
use khal::{
    mem::{p2v, v2p},
    paging::{MappingFlags, PageSize, PageTable, PageTableMut},
};
use ksync::Mutex;
use memaddr::{DynPageIter, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
use memset::MemorySetBackend;

pub mod cow;
pub mod file;
pub mod linear;
pub mod shared;

pub use shared::SharedPages;

use crate::aspace::AddrSpace;

fn divide_page(size: usize, pgsize: PageSize) -> usize {
    assert!(pgsize.is_aligned(size), "unaligned");
    size >> (pgsize as usize).trailing_zeros()
}

fn alloc_frame(zeroed: bool, size: PageSize) -> AxResult<PhysAddr> {
    let pgsize = size as usize;
    let num_pages = pgsize / PAGE_SIZE_4K;
    let vaddr =
        VirtAddr::from(global_allocator().alloc_pages(num_pages, pgsize, UsageKind::VirtMem)?);
    if zeroed {
        unsafe { core::ptr::write_bytes(vaddr.as_mut_ptr(), 0, pgsize) };
    }
    let paddr = v2p(vaddr);

    Ok(paddr)
}

fn dealloc_frame(frame: PhysAddr, align: PageSize) {
    let vaddr = p2v(frame);
    let page_size: usize = align.into();
    let num_pages = page_size / PAGE_SIZE_4K;
    global_allocator().dealloc_pages(vaddr.as_usize(), num_pages, UsageKind::VirtMem);
}

fn pages_in(range: VirtAddrRange, align: PageSize) -> AxResult<DynPageIter<VirtAddr>> {
    DynPageIter::new(range.start, range.end, align as usize).ok_or(AxError::InvalidInput)
}

#[enum_dispatch]
pub trait BackendOps {
    /// Returns the page size of the backend.
    fn page_size(&self) -> PageSize;

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> AxResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> AxResult;

    /// Called before a memory region is protected.
    fn on_protect(
        &self,
        _range: VirtAddrRange,
        _new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> AxResult {
        Ok(())
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

    /// Duplicates this mapping for use in a different page table.
    ///
    /// This differs from `clone`, which is designed for splitting a mapping
    /// within the same table.
    ///
    /// [`BackendOps::map`] will be latter called to the returned backend.
    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<Backend>;
}

type PopulateHook = Box<dyn FnOnce(&mut AddrSpace)>;
type PopulateResult = AxResult<(usize, Option<PopulateHook>)>;

/// A unified enum type for different memory mapping backends.
#[derive(Clone)]
#[enum_dispatch(BackendOps)]
pub enum Backend {
    Linear(linear::LinearBackend),
    Cow(cow::CowBackend),
    Shared(shared::SharedBackend),
    File(file::FileBackend),
}

impl MemorySetBackend for Backend {
    type Addr = VirtAddr;
    type Flags = MappingFlags;
    type PageTable = PageTable;

    fn map(
        &self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        pgtbl: &mut PageTable,
    ) -> bool {
        let range = VirtAddrRange::from_start_size(start, size);
        if let Err(err) = BackendOps::map(self, range, flags, &mut pgtbl.modify()) {
            warn!("Failed to map area: {:?}", err);
            false
        } else {
            true
        }
    }

    fn unmap(&self, start: VirtAddr, size: usize, pgtbl: &mut PageTable) -> bool {
        let range = VirtAddrRange::from_start_size(start, size);
        if let Err(err) = BackendOps::unmap(self, range, &mut pgtbl.modify()) {
            warn!("Failed to unmap area: {:?}", err);
            false
        } else {
            true
        }
    }

    fn protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        pgtbl: &mut Self::PageTable,
    ) -> bool {
        pgtbl
            .modify()
            .protect_region(start, size, new_flags)
            .is_ok()
    }
}
