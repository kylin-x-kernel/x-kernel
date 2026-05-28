// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared mapping backend.
use alloc::{sync::Arc, vec::Vec};
use core::ops::Deref;

use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};

use super::{alloc_frame, dealloc_frame};
use crate::{
    aspace::AddrSpace,
    backend::{Backend, BackendOps, divide_page, map_paging_err, pages_in},
};

/// Shared physical pages backing a mapping.
pub struct SharedPages {
    pub phys_pages: Vec<PhysAddr>,
    pub size: PageSize,
}
impl SharedPages {
    /// Allocate a new set of shared pages.
    pub fn new(size: usize, pgsize: PageSize) -> KResult<Self> {
        let num_pages = divide_page(size, pgsize);
        let mut result = Self {
            phys_pages: Vec::with_capacity(num_pages),
            size: pgsize,
        };
        for _ in 0..num_pages {
            result.phys_pages.push(alloc_frame(true, pgsize)?);
        }
        Ok(result)
    }

    /// Return the number of pages.
    pub fn len(&self) -> usize {
        self.phys_pages.len()
    }

    /// Returns `true` if there are no pages.
    pub fn is_empty(&self) -> bool {
        self.phys_pages.is_empty()
    }
}

impl Deref for SharedPages {
    type Target = [PhysAddr];

    fn deref(&self) -> &Self::Target {
        &self.phys_pages
    }
}

impl Drop for SharedPages {
    fn drop(&mut self) {
        for frame in &self.phys_pages {
            dealloc_frame(*frame, self.size);
        }
    }
}

// FIXME: This implementation does not allow map or unmap partial ranges.
#[derive(Clone)]
pub struct SharedBackend {
    start: VirtAddr,
    pages: Arc<SharedPages>,
}
impl SharedBackend {
    /// Access the shared page set.
    pub fn pages(&self) -> &Arc<SharedPages> {
        &self.pages
    }

    /// Create a relocated shared backend at a new start address with the same pages.
    pub fn relocated(&self, new_start: VirtAddr) -> Backend {
        Backend::Shared(Self {
            start: new_start,
            pages: self.pages.clone(),
        })
    }

    fn pages_starting_from(&self, start: VirtAddr) -> &[PhysAddr] {
        debug_assert!(start.is_aligned(self.pages.size));
        let start_index = divide_page(start - self.start, self.pages.size);
        &self.pages[start_index..]
    }
}

impl BackendOps for SharedBackend {
    fn page_size(&self) -> PageSize {
        self.pages.size
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        debug!("Shared::map: {:?} {:?}", range, flags);
        for (vaddr, paddr) in
            pages_in(range, self.pages.size)?.zip(self.pages_starting_from(range.start))
        {
            pgtbl
                .map(vaddr, *paddr, self.pages.size, flags)
                .map_err(map_paging_err)?;
        }
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        debug!("Shared::unmap: {:?}", range);
        for vaddr in pages_in(range, self.pages.size)? {
            match pgtbl.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(map_paging_err(err)),
            }
        }
        Ok(())
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        Ok(Backend::Shared(self.clone()))
    }
}

impl Backend {
    /// Create a shared mapping backend.
    pub fn new_shared(start: VirtAddr, pages: Arc<SharedPages>) -> Self {
        Self::Shared(SharedBackend { start, pages })
    }

    /// Create an anonymous shared mapping backend, allocating fresh physical pages.
    pub fn new_anonymous_shared(start: VirtAddr, size: usize, pgsize: PageSize) -> KResult<Self> {
        let pages = Arc::new(SharedPages::new(size, pgsize)?);
        Ok(Self::Shared(SharedBackend { start, pages }))
    }
}
