// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory mapping backends.
use alloc::{boxed::Box, sync::Arc};
use core::any::Any;

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::{
    mem::{p2v, v2p},
    paging::{MappingFlags, PageSize, PageTable, PageTableMut, PagingError},
};
use ksync::Mutex;
use memaddr::{DynPageIter, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
use memset::MemorySetBackend;

pub mod linear;
pub mod shared;

pub use shared::SharedPages;

use crate::aspace::AddrSpace;

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

    /// Map a memory region.
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult;

    /// Unmap a memory region.
    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult;

    /// Called before a memory region is protected.
    fn on_protect(
        &self,
        _range: VirtAddrRange,
        _new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult {
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
    ) -> KResult<Backend>;
}

pub trait DynBackendOps: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn page_size(&self) -> PageSize;
    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult;
    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult;
    fn on_protect(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult;
    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> PopulateResult;
    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend>;
    /// Create a relocated copy of this backend at a new start virtual address.
    fn relocated(&self, new_start: VirtAddr, aspace: &Arc<Mutex<AddrSpace>>) -> KResult<Backend>;
    /// Returns `true` if this is an anonymous (non-file-backed) mapping.
    fn is_anonymous(&self) -> bool;
}

#[derive(Clone)]
pub struct DynamicBackend(Arc<dyn DynBackendOps>);

impl DynamicBackend {
    pub fn new(backend: Arc<dyn DynBackendOps>) -> Self {
        Self(backend)
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.0.as_any().downcast_ref::<T>()
    }
}

type PopulateHook = Box<dyn FnOnce(&mut AddrSpace)>;
type PopulateResult = KResult<(usize, Option<PopulateHook>)>;

/// A unified enum type for different memory mapping backends.
#[derive(Clone)]
pub enum Backend {
    Linear(linear::LinearBackend),
    Shared(shared::SharedBackend),
    Dynamic(DynamicBackend),
}

impl Backend {
    pub fn new_dynamic(backend: Arc<dyn DynBackendOps>) -> Self {
        Self::Dynamic(DynamicBackend::new(backend))
    }

    pub fn downcast_dynamic_ref<T: 'static>(&self) -> Option<&T> {
        match self {
            Self::Dynamic(dynamic) => dynamic.downcast_ref::<T>(),
            _ => None,
        }
    }

    /// Create a relocated copy of this backend at a new start address.
    ///
    /// Used by mremap to move a mapping to a different virtual address while
    /// preserving the backend's physical pages and semantics.
    ///
    /// Returns `OperationNotSupported` for linear backends (their VA-to-PA
    /// offset is fixed and cannot be relocated).
    pub fn relocated(
        &self,
        new_start: VirtAddr,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        match self {
            Self::Linear(_) => Err(KError::OperationNotSupported),
            Self::Shared(inner) => Ok(inner.relocated(new_start)),
            Self::Dynamic(inner) => inner.0.relocated(new_start, aspace),
        }
    }

    /// Returns `true` if this is an anonymous (non-file-backed) mapping.
    pub fn is_anonymous(&self) -> bool {
        match self {
            Self::Linear(_) => false,
            Self::Shared(_) => true,
            Self::Dynamic(inner) => inner.0.is_anonymous(),
        }
    }
}

impl BackendOps for Backend {
    fn page_size(&self) -> PageSize {
        match self {
            Self::Linear(inner) => inner.page_size(),
            Self::Shared(inner) => inner.page_size(),
            Self::Dynamic(inner) => inner.0.page_size(),
        }
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        match self {
            Self::Linear(inner) => inner.map(range, flags, pgtbl),
            Self::Shared(inner) => inner.map(range, flags, pgtbl),
            Self::Dynamic(inner) => inner.0.map(range, flags, pgtbl),
        }
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        match self {
            Self::Linear(inner) => inner.unmap(range, pgtbl),
            Self::Shared(inner) => inner.unmap(range, pgtbl),
            Self::Dynamic(inner) => inner.0.unmap(range, pgtbl),
        }
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        new_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult {
        match self {
            Self::Linear(inner) => inner.on_protect(range, new_flags, pgtbl),
            Self::Shared(inner) => inner.on_protect(range, new_flags, pgtbl),
            Self::Dynamic(inner) => inner.0.on_protect(range, new_flags, pgtbl),
        }
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> PopulateResult {
        match self {
            Self::Linear(inner) => inner.populate(range, flags, access_flags, pgtbl),
            Self::Shared(inner) => inner.populate(range, flags, access_flags, pgtbl),
            Self::Dynamic(inner) => inner.0.populate(range, flags, access_flags, pgtbl),
        }
    }

    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        match self {
            Self::Linear(inner) => inner.clone_map(range, flags, old_pgtbl, new_pgtbl, new_aspace),
            Self::Shared(inner) => inner.clone_map(range, flags, old_pgtbl, new_pgtbl, new_aspace),
            Self::Dynamic(inner) => inner
                .0
                .clone_map(range, flags, old_pgtbl, new_pgtbl, new_aspace),
        }
    }
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
        let range = VirtAddrRange::from_start_size(start, size);
        let mut modifier = pgtbl.modify();
        if let Err(err) = BackendOps::on_protect(self, range, new_flags, &mut modifier) {
            warn!("Failed to protect area: {:?}", err);
            return false;
        }
        modifier.protect_region(start, size, new_flags).is_ok()
    }
}
